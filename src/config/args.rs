use alloy_dyn_abi::{DynSolType, DynSolValue, Specifier};
use alloy_json_abi::Function;
use alloy_primitives::{Address, FixedBytes, I256, U256};
use anyhow::{Context, Result, anyhow, bail};
use serde_yaml_ng::Value;

/// Converts YAML-decoded args into ABI values matching `func`'s input types.
/// Mirrors Go `ConvertArgs` (config/args.go) for the supported scalar set:
/// address, bool, uint<M>, int<M>, string, bytes, bytes<N>.
pub fn convert_args(func: &Function, args: &[Value]) -> Result<Vec<DynSolValue>> {
    if args.len() != func.inputs.len() {
        bail!(
            "function {:?} expects {} argument(s), got {}",
            func.name,
            func.inputs.len(),
            args.len()
        );
    }
    let mut out = Vec::with_capacity(args.len());
    for (i, param) in func.inputs.iter().enumerate() {
        let ty: DynSolType = param
            .resolve()
            .with_context(|| format!("resolve type of args[{i}] ({})", param.ty))?;
        let v = convert_one(&ty, &args[i]).with_context(|| format!("args[{i}] ({})", param.ty))?;
        out.push(v);
    }
    Ok(out)
}

fn convert_one(ty: &DynSolType, v: &Value) -> Result<DynSolValue> {
    match ty {
        DynSolType::Address => Ok(DynSolValue::Address(parse_address(v)?)),
        DynSolType::Bool => Ok(DynSolValue::Bool(as_bool(v)?)),
        DynSolType::Uint(bits) => Ok(DynSolValue::Uint(as_u256(v)?, *bits)),
        DynSolType::Int(bits) => Ok(DynSolValue::Int(as_i256(v)?, *bits)),
        DynSolType::String => Ok(DynSolValue::String(as_string(v)?)),
        DynSolType::Bytes => Ok(DynSolValue::Bytes(as_bytes(v)?)),
        DynSolType::FixedBytes(n) => {
            let b = as_bytes(v)?;
            if b.len() != *n {
                bail!("bytes{n} expects {n} bytes, got {}", b.len());
            }
            let mut word = [0u8; 32];
            word[..*n].copy_from_slice(&b);
            Ok(DynSolValue::FixedBytes(FixedBytes::<32>::from(word), *n))
        }
        other => bail!("unsupported argument type {other:?}"),
    }
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}

fn parse_address(v: &Value) -> Result<Address> {
    let s = as_string(v)?;
    s.parse::<Address>()
        .with_context(|| format!("invalid address {s:?}"))
}

fn as_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => bail!("expected bool, got {other:?}"),
    }
}

fn as_string(v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        other => bail!("expected string scalar, got {other:?}"),
    }
}

fn as_u256(v: &Value) -> Result<U256> {
    match v {
        Value::Number(n) => {
            U256::from_str_radix(&n.to_string(), 10).map_err(|e| anyhow!("invalid uint {n}: {e}"))
        }
        Value::String(s) => {
            let t = s.trim();
            if t.starts_with("0x") || t.starts_with("0X") {
                U256::from_str_radix(strip_0x(t), 16)
                    .map_err(|e| anyhow!("invalid hex uint {s:?}: {e}"))
            } else {
                U256::from_str_radix(t, 10).map_err(|e| anyhow!("invalid uint {s:?}: {e}"))
            }
        }
        other => bail!("expected uint scalar, got {other:?}"),
    }
}

fn as_i256(v: &Value) -> Result<I256> {
    let s = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        other => bail!("expected int scalar, got {other:?}"),
    };
    s.parse::<I256>()
        .map_err(|e| anyhow!("invalid int {s:?}: {e}"))
}

fn as_bytes(v: &Value) -> Result<Vec<u8>> {
    let s = as_string(v)?;
    alloy_primitives::hex::decode(strip_0x(s.trim()))
        .with_context(|| format!("invalid hex bytes {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_abi::JsonAbi;

    fn func(sig_abi: &str, name: &str) -> Function {
        serde_json::from_str::<JsonAbi>(sig_abi)
            .expect("abi")
            .function(name)
            .expect("fn")[0]
            .clone()
    }

    #[test]
    fn converts_address() {
        let f = func(
            r#"[{"type":"function","name":"f","inputs":[{"name":"a","type":"address"}],"outputs":[],"stateMutability":"view"}]"#,
            "f",
        );
        let args = vec![Value::String(
            "0x0000000000000000000000000000000000000001".into(),
        )];
        let got = convert_args(&f, &args).expect("convert");
        assert_eq!(
            got,
            vec![DynSolValue::Address(
                "0x0000000000000000000000000000000000000001"
                    .parse()
                    .expect("addr")
            )]
        );
    }

    #[test]
    fn converts_uint_from_number_and_string() {
        let f = func(
            r#"[{"type":"function","name":"f","inputs":[{"name":"x","type":"uint256"}],"outputs":[],"stateMutability":"view"}]"#,
            "f",
        );
        let from_num = convert_args(&f, &[Value::Number(42.into())]).expect("num");
        let from_dec = convert_args(&f, &[Value::String("42".into())]).expect("dec");
        let from_hex = convert_args(&f, &[Value::String("0x2a".into())]).expect("hex");
        assert_eq!(from_num, vec![DynSolValue::Uint(U256::from(42), 256)]);
        assert_eq!(from_dec, from_num);
        assert_eq!(from_hex, from_num);
    }

    #[test]
    fn converts_bool() {
        let f = func(
            r#"[{"type":"function","name":"f","inputs":[{"name":"b","type":"bool"}],"outputs":[],"stateMutability":"view"}]"#,
            "f",
        );
        assert_eq!(
            convert_args(&f, &[Value::Bool(true)]).expect("bool"),
            vec![DynSolValue::Bool(true)]
        );
    }

    #[test]
    fn converts_fixed_bytes() {
        let f = func(
            r#"[{"type":"function","name":"f","inputs":[{"name":"s","type":"bytes4"}],"outputs":[],"stateMutability":"view"}]"#,
            "f",
        );
        let got = convert_args(&f, &[Value::String("0xdeadbeef".into())]).expect("b4");
        let mut word = [0u8; 32];
        word[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            got,
            vec![DynSolValue::FixedBytes(FixedBytes::<32>::from(word), 4)]
        );
    }

    #[test]
    fn rejects_arg_count_mismatch() {
        let f = func(
            r#"[{"type":"function","name":"f","inputs":[{"name":"x","type":"uint256"}],"outputs":[],"stateMutability":"view"}]"#,
            "f",
        );
        assert!(convert_args(&f, &[]).is_err());
    }

    #[test]
    fn rejects_wrong_kind() {
        let f = func(
            r#"[{"type":"function","name":"f","inputs":[{"name":"a","type":"address"}],"outputs":[],"stateMutability":"view"}]"#,
            "f",
        );
        assert!(convert_args(&f, &[Value::Bool(true)]).is_err());
    }
}
