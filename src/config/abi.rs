use alloy_dyn_abi::DynSolType;
use alloy_json_abi::{Function, JsonAbi, StateMutability};
use anyhow::{Result, anyhow, bail};

/// Looks up `name` in the ABI and enforces view/pure (Go rules 2 & 3).
/// On overloaded names returns the first overload (matches Go map semantics
/// where a single Method is stored per name).
pub fn resolve_function<'a>(abi: &'a JsonAbi, name: &str) -> Result<&'a Function> {
    let overloads = abi
        .function(name)
        .ok_or_else(|| anyhow!("function {name:?} not found in ABI"))?;
    let func = overloads
        .first()
        .ok_or_else(|| anyhow!("function {name:?} not found in ABI"))?;
    match func.state_mutability {
        StateMutability::View | StateMutability::Pure => Ok(func),
        other => bail!("function {name:?} must be view or pure (got {other:?})"),
    }
}

/// Resolves a tuple-field selector against the tuple output at `output_index`.
/// Returns (field_index, field_name). Mirrors Go `ResolveTupleField`.
pub fn resolve_tuple_field(
    func: &Function,
    output_index: usize,
    field: &str,
) -> Result<(usize, String)> {
    let param = func
        .outputs
        .get(output_index)
        .ok_or_else(|| anyhow!("output index {output_index} out of range"))?;
    if !param.ty.starts_with("tuple") || param.components.is_empty() {
        bail!(
            "field {field:?} selected but output {output_index} of {:?} is {}, not a tuple",
            func.name,
            param.ty
        );
    }
    for (i, c) in param.components.iter().enumerate() {
        if c.name == field {
            return Ok((i, c.name.clone()));
        }
    }
    let available: Vec<&str> = param.components.iter().map(|c| c.name.as_str()).collect();
    bail!(
        "tuple field {field:?} not found in output {output_index} of {:?}; available: {available:?}",
        func.name
    )
}

/// Whether a resolved ABI type is a numeric/bool scalar (uint/int/bool).
/// Used by config validation for tuple-field outputs.
pub fn output_scalar_is_numeric(ty: &DynSolType) -> bool {
    matches!(
        ty,
        DynSolType::Uint(_) | DynSolType::Int(_) | DynSolType::Bool
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABI: &str = r#"[
      {"type":"function","name":"apy","inputs":[],"outputs":[{"name":"annualYield","type":"uint256"}],"stateMutability":"view"},
      {"type":"function","name":"poke","inputs":[],"outputs":[],"stateMutability":"nonpayable"},
      {"type":"function","name":"slot0","inputs":[],"outputs":[{"name":"","type":"tuple","components":[{"name":"price","type":"uint160"},{"name":"tick","type":"int24"}]}],"stateMutability":"view"}
    ]"#;

    fn abi() -> JsonAbi {
        serde_json::from_str::<JsonAbi>(ABI).expect("valid abi")
    }

    #[test]
    fn resolve_function_ok_for_view() {
        let a = abi();
        assert_eq!(resolve_function(&a, "apy").expect("found").name, "apy");
    }

    #[test]
    fn resolve_function_rejects_non_view() {
        let a = abi();
        assert!(resolve_function(&a, "poke").is_err());
    }

    #[test]
    fn resolve_function_missing() {
        let a = abi();
        assert!(resolve_function(&a, "nope").is_err());
    }

    #[test]
    fn resolve_tuple_field_by_name() {
        let a = abi();
        let f = resolve_function(&a, "slot0").expect("found");
        let (idx, name) = resolve_tuple_field(f, 0, "tick").expect("resolved");
        assert_eq!((idx, name.as_str()), (1, "tick"));
    }

    #[test]
    fn resolve_tuple_field_unknown() {
        let a = abi();
        let f = resolve_function(&a, "slot0").expect("found");
        assert!(resolve_tuple_field(f, 0, "nope").is_err());
    }
}
