use alloy_json_abi::Function;
use anyhow::{Result, anyhow};

use crate::config::abi::resolve_tuple_field;
use crate::config::{Metric, Output};

/// Converts camelCase / PascalCase identifiers to snake_case.
/// Acronym-aware: a boundary is inserted before a trailing uppercase only when
/// it is followed by a lowercase (so `APIKey` -> `api_key`, `feeAPR` -> `fee_apr`).
/// Mirrors Go `CamelToSnake`.
pub fn camel_to_snake(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let runes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &r) in runes.iter().enumerate() {
        if i > 0 && r.is_uppercase() {
            let prev = runes[i - 1];
            let next = runes.get(i + 1).copied();
            let after_lower_or_digit = prev.is_lowercase() || prev.is_ascii_digit();
            let acronym_to_word =
                prev.is_uppercase() && matches!(next, Some(n) if n.is_lowercase());
            if after_lower_or_digit || acronym_to_word {
                out.push('_');
            }
        }
        for lc in r.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

/// Returns the metric name(s) this Metric resolves to. Direct port of Go
/// `InferMetricNames` (config.go §4.3/§5.1).
pub fn infer_metric_names(m: &Metric, func: &Function) -> Result<Vec<String>> {
    let base = if m.name.is_empty() {
        camel_to_snake(&func.name)
    } else {
        m.name.clone()
    };

    let selected: Vec<Output> = if m.outputs.is_empty() {
        if func.outputs.len() == 1 {
            return Ok(vec![base]);
        }
        (0..func.outputs.len())
            .map(|i| Output {
                index: i,
                ..Default::default()
            })
            .collect()
    } else {
        m.outputs.clone()
    };

    let mut names = Vec::with_capacity(selected.len());
    for o in &selected {
        let out = func
            .outputs
            .get(o.index)
            .ok_or_else(|| anyhow!("output index {} out of range", o.index))?;

        if !o.field.is_empty() {
            let suffix = if !o.name.is_empty() {
                o.name.clone()
            } else {
                let (_, fname) = resolve_tuple_field(func, o.index, &o.field)?;
                if fname.is_empty() {
                    o.field.clone()
                } else {
                    fname
                }
            };
            names.push(format!("{base}_{}", camel_to_snake(&suffix)));
            continue;
        }
        if !o.name.is_empty() {
            if func.outputs.len() == 1 {
                names.push(camel_to_snake(&o.name));
            } else {
                names.push(format!("{base}_{}", camel_to_snake(&o.name)));
            }
            continue;
        }
        if func.outputs.len() == 1 {
            names.push(base.clone());
            continue;
        }
        if !out.name.is_empty() {
            names.push(format!("{base}_{}", camel_to_snake(&out.name)));
        } else {
            names.push(format!("{base}_{}", o.index));
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_examples() {
        assert_eq!(camel_to_snake("latestRoundData"), "latest_round_data");
        assert_eq!(camel_to_snake("balanceOf"), "balance_of");
        assert_eq!(camel_to_snake("getReserves"), "get_reserves");
        assert_eq!(camel_to_snake("feeAPR"), "fee_apr");
        assert_eq!(camel_to_snake("rateLimitV2"), "rate_limit_v2");
        assert_eq!(camel_to_snake("latestRound"), "latest_round");
        assert_eq!(camel_to_snake("APIKey"), "api_key");
        assert_eq!(camel_to_snake("SECONDS_PER_YEAR"), "seconds_per_year");
        assert_eq!(camel_to_snake(""), "");
        assert_eq!(camel_to_snake("apy"), "apy");
    }
}

#[cfg(test)]
mod infer_tests {
    use super::*;
    use crate::config::Metric;
    use alloy_json_abi::JsonAbi;

    const ABI: &str = r#"[
      {"type":"function","name":"apy","inputs":[],"outputs":[{"name":"annualYield","type":"uint256"}],"stateMutability":"view"},
      {"type":"function","name":"getReserves","inputs":[],"outputs":[{"name":"reserve0","type":"uint112"},{"name":"reserve1","type":"uint112"},{"name":"blockTimestampLast","type":"uint32"}],"stateMutability":"view"},
      {"type":"function","name":"slot0","inputs":[],"outputs":[{"name":"","type":"tuple","components":[{"name":"sqrtPriceX96","type":"uint160"},{"name":"tick","type":"int24"}]}],"stateMutability":"view"}
    ]"#;

    fn func(name: &str) -> Function {
        serde_json::from_str::<JsonAbi>(ABI)
            .expect("abi")
            .function(name)
            .expect("fn")[0]
            .clone()
    }

    #[test]
    fn single_output_uses_function_name() {
        let m = Metric {
            function: "apy".into(),
            ..Default::default()
        };
        assert_eq!(
            infer_metric_names(&m, &func("apy")).expect("names"),
            vec!["apy"]
        );
    }

    #[test]
    fn single_output_explicit_name_replaces() {
        let m = Metric {
            function: "apy".into(),
            outputs: vec![Output {
                name: "annual_yield".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            infer_metric_names(&m, &func("apy")).expect("names"),
            vec!["annual_yield"]
        );
    }

    #[test]
    fn multi_output_appends_output_names() {
        let m = Metric {
            function: "getReserves".into(),
            ..Default::default()
        };
        assert_eq!(
            infer_metric_names(&m, &func("getReserves")).expect("names"),
            vec![
                "get_reserves_reserve0",
                "get_reserves_reserve1",
                "get_reserves_block_timestamp_last"
            ]
        );
    }

    #[test]
    fn metric_name_override_is_base() {
        let m = Metric {
            name: "reserves".into(),
            function: "getReserves".into(),
            ..Default::default()
        };
        assert_eq!(
            infer_metric_names(&m, &func("getReserves")).expect("names"),
            vec![
                "reserves_reserve0",
                "reserves_reserve1",
                "reserves_block_timestamp_last"
            ]
        );
    }

    #[test]
    fn tuple_field_suffix() {
        let m = Metric {
            function: "slot0".into(),
            outputs: vec![Output {
                index: 0,
                field: "tick".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            infer_metric_names(&m, &func("slot0")).expect("names"),
            vec!["slot0_tick"]
        );
    }
}
