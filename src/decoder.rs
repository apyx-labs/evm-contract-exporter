use alloy_dyn_abi::{DynSolValue, FunctionExt};
use alloy_json_abi::Function;
use anyhow::{Result, anyhow, bail};
use rust_decimal::Decimal;
use rust_decimal::prelude::*;

/// Everything the decoder needs to extract one gauge value from a return blob.
#[derive(Debug, Clone)]
pub struct DecodeTarget {
    pub function: Function,
    pub output_index: usize,
    pub field_index: Option<usize>,
    pub scale: f64,
}

#[derive(Debug, thiserror::Error)]
#[error("decoder: unsupported return type for numeric metric: {0}")]
pub struct UnsupportedType(pub String);

/// Decodes a single f64 gauge value from a Multicall3 return blob.
/// Returns (value, precision_loss). Mirrors Go `decoder.Decode`.
pub fn decode(return_data: &[u8], target: &DecodeTarget) -> Result<(f64, bool)> {
    let outputs = &target.function.outputs;
    if outputs.is_empty() {
        bail!("decoder: method {:?} has no outputs", target.function.name);
    }
    if target.output_index >= outputs.len() {
        bail!(
            "decoder: output index {} out of range for {:?} ({} outputs)",
            target.output_index,
            target.function.name,
            outputs.len()
        );
    }
    let decoded = target
        .function
        .abi_decode_output(return_data)
        .map_err(|e| anyhow!("decoder: unpack {:?}: {e}", target.function.name))?;
    let mut value = decoded
        .get(target.output_index)
        .ok_or_else(|| {
            anyhow!(
                "decoder: unpacked {} values, need index {}",
                decoded.len(),
                target.output_index
            )
        })?
        .clone();

    if let Some(fi) = target.field_index {
        value = match value {
            DynSolValue::Tuple(fields) => fields.into_iter().nth(fi).ok_or_else(|| {
                anyhow!(
                    "decoder: tuple field index {fi} out of range for {:?}",
                    target.function.name
                )
            })?,
            other => bail!(
                "decoder: field set but output {} of {:?} is not a tuple ({other:?})",
                target.output_index,
                target.function.name
            ),
        };
    }

    let scale = if target.scale == 0.0 {
        1.0
    } else {
        target.scale
    };
    convert_to_f64(&value, scale)
}

fn convert_to_f64(v: &DynSolValue, scale: f64) -> Result<(f64, bool)> {
    match v {
        DynSolValue::Uint(u, _) => Ok(scale_decimal(&u.to_string(), scale)),
        DynSolValue::Int(i, _) => Ok(scale_decimal(&i.to_string(), scale)),
        DynSolValue::Bool(b) => Ok((if *b { 1.0 } else { 0.0 }, false)),
        other => Err(UnsupportedType(format!("{other:?}")).into()),
    }
}

/// Scales a decimal-string integer by `scale`, returning (f64, precision_loss).
/// Uses rust_decimal when the magnitude fits (~28 digits); otherwise falls back
/// to a string→f64 parse and flags precision loss.
fn scale_decimal(int_str: &str, scale: f64) -> (f64, bool) {
    if let (Ok(dec_v), Some(dec_scale)) = (Decimal::from_str(int_str), Decimal::from_f64(scale))
        && !dec_scale.is_zero()
        && let Some(q) = dec_v.checked_div(dec_scale)
    {
        let f = q.to_f64().unwrap_or(f64::NAN);
        // Round-trip back to Decimal: differs ⇒ the f64 lost precision.
        let loss = Decimal::from_f64(f).map(|d| d != q).unwrap_or(true);
        return (f, loss);
    }
    // Fallback: value too large for Decimal (or scale not representable).
    let parsed = int_str.parse::<f64>().unwrap_or(f64::NAN);
    (parsed / scale, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_abi::JsonAbi;
    use alloy_primitives::U256;

    fn func(abi: &str, name: &str) -> Function {
        serde_json::from_str::<JsonAbi>(abi)
            .expect("abi")
            .function(name)
            .expect("fn")[0]
            .clone()
    }

    fn encode_output(f: &Function, vals: &[DynSolValue]) -> Vec<u8> {
        f.abi_encode_output(vals).expect("encode")
    }

    #[test]
    fn decodes_uint_with_scale() {
        let abi = r#"[{"type":"function","name":"apr","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]"#;
        let f = func(abi, "apr");
        let data = encode_output(
            &f,
            &[DynSolValue::Uint(
                U256::from(50_000_000_000_000_000u64),
                256,
            )],
        );
        let target = DecodeTarget {
            function: f,
            output_index: 0,
            field_index: None,
            scale: 1e18,
        };
        let (v, _loss) = decode(&data, &target).expect("decode");
        assert!((v - 0.05).abs() < 1e-12);
    }

    #[test]
    fn decodes_bool() {
        let abi = r#"[{"type":"function","name":"ok","inputs":[],"outputs":[{"name":"","type":"bool"}],"stateMutability":"view"}]"#;
        let f = func(abi, "ok");
        let data = encode_output(&f, &[DynSolValue::Bool(true)]);
        let target = DecodeTarget {
            function: f,
            output_index: 0,
            field_index: None,
            scale: 1.0,
        };
        let (v, loss) = decode(&data, &target).expect("decode");
        assert_eq!((v, loss), (1.0, false));
    }

    #[test]
    fn rejects_address_output() {
        let abi = r#"[{"type":"function","name":"vault","inputs":[],"outputs":[{"name":"","type":"address"}],"stateMutability":"view"}]"#;
        let f = func(abi, "vault");
        let data = encode_output(&f, &[DynSolValue::Address(alloy_primitives::Address::ZERO)]);
        let target = DecodeTarget {
            function: f,
            output_index: 0,
            field_index: None,
            scale: 1.0,
        };
        assert!(decode(&data, &target).is_err());
    }

    #[test]
    fn selects_tuple_field() {
        let abi = r#"[{"type":"function","name":"slot0","inputs":[],"outputs":[{"name":"","type":"tuple","components":[{"name":"price","type":"uint160"},{"name":"tick","type":"int24"}]}],"stateMutability":"view"}]"#;
        let f = func(abi, "slot0");
        let tuple = DynSolValue::Tuple(vec![
            DynSolValue::Uint(U256::from(1000u64), 160),
            DynSolValue::Int(alloy_primitives::I256::try_from(-42i64).expect("i256"), 24),
        ]);
        let data = encode_output(&f, &[tuple]);
        let target = DecodeTarget {
            function: f,
            output_index: 0,
            field_index: Some(1),
            scale: 1.0,
        };
        let (v, _loss) = decode(&data, &target).expect("decode");
        assert_eq!(v, -42.0);
    }

    #[test]
    fn flags_precision_loss_for_huge_uint() {
        let abi = r#"[{"type":"function","name":"big","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]"#;
        let f = func(abi, "big");
        let huge = U256::from(1u64) << 200;
        let data = encode_output(&f, &[DynSolValue::Uint(huge, 256)]);
        let target = DecodeTarget {
            function: f,
            output_index: 0,
            field_index: None,
            scale: 1.0,
        };
        let (_v, loss) = decode(&data, &target).expect("decode");
        assert!(loss);
    }
}
