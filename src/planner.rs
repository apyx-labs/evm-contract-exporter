use std::collections::{BTreeMap, BTreeSet, HashMap};

use alloy_dyn_abi::JsonAbiExt;
use alloy_json_abi::Function;
use alloy_primitives::Address;
use anyhow::{Context, Result, anyhow, bail};
use prometheus::{GaugeVec, Opts, Registry};

use crate::config::abi::resolve_tuple_field;
use crate::config::args::convert_args;
use crate::config::naming::infer_metric_names;
use crate::config::{Config, Metric, Output};
use crate::decoder::DecodeTarget;
use crate::multicall::CallRequest;

/// A single emitted metric series — one gauge sample per scrape.
#[derive(Clone)]
pub struct Entry {
    pub call_index: usize,
    pub decode: DecodeTarget,
    pub metric_name: String,
    pub gauge: GaugeVec,
    /// Registered label-key order for this entry's gauge (exposed via
    /// [`Entry::gauge_label_keys`]); the scrape loop addresses gauges by value.
    pub(crate) label_keys: Vec<String>,
    pub label_values: Vec<String>,
    pub contract_name: String,
    pub function_name: String,
    pub address: Address,
}

impl Entry {
    /// Returns the registered label-key order for this entry's gauge.
    pub fn gauge_label_keys(&self) -> Vec<String> {
        self.label_keys.clone()
    }
}

pub struct Plan {
    pub calls: Vec<CallRequest>,
    pub entries: Vec<Entry>,
    pub chunk_count: usize,
}

struct Subcall<'a> {
    function: &'a str,
    args: &'a [serde_yaml_ng::Value],
    call_labels: &'a crate::config::Labels,
    outputs: &'a [Output],
}

fn expand_metric_calls(m: &Metric) -> Vec<Subcall<'_>> {
    if !m.calls.is_empty() {
        m.calls
            .iter()
            .map(|c| Subcall {
                function: &m.function,
                args: &c.args,
                call_labels: &c.labels,
                outputs: &m.outputs,
            })
            .collect()
    } else {
        static EMPTY_LABELS: std::sync::LazyLock<crate::config::Labels> =
            std::sync::LazyLock::new(BTreeMap::new);
        static EMPTY_ARGS: &[serde_yaml_ng::Value] = &[];
        vec![Subcall {
            function: &m.function,
            args: if m.args.is_empty() {
                EMPTY_ARGS
            } else {
                &m.args
            },
            call_labels: &EMPTY_LABELS,
            outputs: &m.outputs,
        }]
    }
}

fn resolve_outputs(
    m: &Metric,
    sc: &Subcall,
    func: &Function,
) -> Result<(Vec<String>, Vec<Output>)> {
    let surrogate = Metric {
        name: m.name.clone(),
        help: m.help.clone(),
        outputs: sc.outputs.to_vec(),
        ..Default::default()
    };
    let names = infer_metric_names(&surrogate, func)?;
    let outs: Vec<Output> = if sc.outputs.is_empty() {
        if func.outputs.len() == 1 {
            vec![Output {
                index: 0,
                ..Default::default()
            }]
        } else {
            (0..func.outputs.len())
                .map(|i| Output {
                    index: i,
                    ..Default::default()
                })
                .collect()
        }
    } else {
        sc.outputs.to_vec()
    };
    if names.len() != outs.len() {
        bail!(
            "planner: output count mismatch ({} names vs {} outputs)",
            names.len(),
            outs.len()
        );
    }
    Ok((names, outs))
}

struct Draft {
    call_index: usize,
    decode: DecodeTarget,
    metric_name: String,
    user_labels: BTreeMap<String, String>,
    address: Address,
    contract_name: String,
    function_name: String,
}

pub fn build(cfg: &Config, registry: &Registry) -> Result<Plan> {
    let mut calls: Vec<CallRequest> = Vec::new();
    let mut call_by_key: HashMap<(String, String), usize> = HashMap::new();
    let mut drafts: Vec<Draft> = Vec::new();

    let mut help_by_metric: HashMap<String, String> = HashMap::new();
    let mut keys_by_metric: HashMap<String, BTreeSet<String>> = HashMap::new();

    let chain_id = cfg.chain.chain_id;

    for ct in &cfg.contracts {
        let parsed = ct.parsed_abi.as_ref().ok_or_else(|| {
            anyhow!(
                "contract {:?} has no parsed ABI (config not validated?)",
                ct.name
            )
        })?;
        for inst in &ct.instances {
            let addr: Address = inst.address.trim().parse().map_err(|_| {
                anyhow!(
                    "contract {:?} instance {:?}: invalid address",
                    ct.name,
                    inst.address
                )
            })?;
            for m in &ct.metrics {
                for sc in expand_metric_calls(m) {
                    let overloads = parsed.function(sc.function).ok_or_else(|| {
                        anyhow!(
                            "contract {:?} metric {:?}: function {:?} missing from ABI",
                            ct.name,
                            crate::config::metric_descriptor(m),
                            sc.function
                        )
                    })?;
                    let func = &overloads[0];

                    let packed = convert_args(func, sc.args).with_context(|| {
                        format!(
                            "contract {:?} metric {:?}",
                            ct.name,
                            crate::config::metric_descriptor(m)
                        )
                    })?;
                    let call_data = func.abi_encode_input(&packed).map_err(|e| {
                        anyhow!(
                            "contract {:?} metric {:?}: pack {:?}: {e}",
                            ct.name,
                            crate::config::metric_descriptor(m),
                            sc.function
                        )
                    })?;
                    let call_data: alloy_primitives::Bytes = call_data.into();

                    let key = (
                        format!("{addr:#x}"),
                        format!("0x{}", alloy_primitives::hex::encode(&call_data)),
                    );
                    let call_index = *call_by_key.entry(key).or_insert_with(|| {
                        let idx = calls.len();
                        calls.push(CallRequest {
                            target: addr,
                            call_data: call_data.clone(),
                        });
                        idx
                    });

                    let (mut names, outputs) =
                        resolve_outputs(m, &sc, func).with_context(|| {
                            format!(
                                "contract {:?} metric {:?}",
                                ct.name,
                                crate::config::metric_descriptor(m)
                            )
                        })?;

                    let prefix = ct.effective_metric_prefix();
                    if !prefix.is_empty() {
                        for n in names.iter_mut() {
                            *n = format!("{prefix}_{n}");
                        }
                    }

                    // Merge labels: top < contract < instance < call.
                    let mut merged: BTreeMap<String, String> = BTreeMap::new();
                    for (k, v) in &cfg.labels {
                        merged.insert(k.clone(), v.clone());
                    }
                    for (k, v) in &ct.labels {
                        merged.insert(k.clone(), v.clone());
                    }
                    for (k, v) in &inst.labels {
                        merged.insert(k.clone(), v.clone());
                    }
                    for (k, v) in sc.call_labels {
                        merged.insert(k.clone(), v.clone());
                    }

                    for (j, name) in names.into_iter().enumerate() {
                        let out = &outputs[j];
                        let field_index = if out.field.is_empty() {
                            None
                        } else {
                            let (fi, _) = resolve_tuple_field(func, out.index, &out.field)
                                .with_context(|| {
                                    format!(
                                        "contract {:?} metric {:?}",
                                        ct.name,
                                        crate::config::metric_descriptor(m)
                                    )
                                })?;
                            Some(fi)
                        };

                        match help_by_metric.get(&name) {
                            None => {
                                help_by_metric.insert(name.clone(), m.help.clone());
                            }
                            Some(existing)
                                if !m.help.is_empty()
                                    && !existing.is_empty()
                                    && existing != &m.help =>
                            {
                                bail!(
                                    "metric {name:?} has conflicting Help texts across contracts"
                                );
                            }
                            Some(existing) if existing.is_empty() && !m.help.is_empty() => {
                                help_by_metric.insert(name.clone(), m.help.clone());
                            }
                            _ => {}
                        }
                        let key_set = keys_by_metric.entry(name.clone()).or_default();
                        for k in merged.keys() {
                            key_set.insert(k.clone());
                        }

                        drafts.push(Draft {
                            call_index,
                            decode: DecodeTarget {
                                function: func.clone(),
                                output_index: out.index,
                                field_index,
                                scale: out.effective_scale(),
                            },
                            metric_name: name,
                            user_labels: merged.clone(),
                            address: addr,
                            contract_name: ct.name.clone(),
                            function_name: sc.function.to_string(),
                        });
                    }
                }
            }
        }
    }

    // Register a GaugeVec per metric name with the union schema.
    let mut gauges: HashMap<String, GaugeVec> = HashMap::new();
    let mut final_keys: HashMap<String, Vec<String>> = HashMap::new();
    for (name, key_set) in &keys_by_metric {
        let mut keys: Vec<String> = key_set.iter().cloned().collect();
        keys.push("address".into());
        keys.push("chain_id".into());
        final_keys.insert(name.clone(), keys.clone());

        let help = help_by_metric
            .get(name)
            .filter(|h| !h.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("EVM contract metric {name}"));
        let label_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let gauge = GaugeVec::new(Opts::new(name.clone(), help), &label_refs)
            .with_context(|| format!("create gauge {name:?}"))?;
        registry
            .register(Box::new(gauge.clone()))
            .with_context(|| format!("register {name:?}"))?;
        gauges.insert(name.clone(), gauge);
    }

    let chain_id_str = chain_id.to_string();
    let mut entries = Vec::with_capacity(drafts.len());
    for d in drafts {
        let keys = &final_keys[&d.metric_name];
        let values: Vec<String> = keys
            .iter()
            .map(|k| match k.as_str() {
                "address" => format!("{:#x}", d.address),
                "chain_id" => chain_id_str.clone(),
                other => d.user_labels.get(other).cloned().unwrap_or_default(),
            })
            .collect();
        entries.push(Entry {
            call_index: d.call_index,
            decode: d.decode,
            metric_name: d.metric_name.clone(),
            gauge: gauges[&d.metric_name].clone(),
            label_keys: keys.clone(),
            label_values: values,
            contract_name: d.contract_name,
            function_name: d.function_name,
            address: d.address,
        });
    }

    let chunk_count = if calls.is_empty() {
        0
    } else {
        let batch = if cfg.scrape.max_calls_per_batch == 0 {
            calls.len()
        } else {
            cfg.scrape.max_calls_per_batch
        };
        calls.len().div_ceil(batch)
    };

    Ok(Plan {
        calls,
        entries,
        chunk_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_str;
    use std::path::Path;

    fn build_from(yaml: &str) -> (Plan, Registry) {
        unsafe { std::env::set_var("TEST_RPC_URL", "http://localhost:8545") };
        let mut cfg = parse_str(yaml).expect("parse");
        cfg.validate(Path::new(".")).expect("validate");
        let reg = Registry::new();
        let plan = build(&cfg, &reg).expect("plan");
        (plan, reg)
    }

    const ABI: &str = r#"[{"type":"function","name":"balanceOf","inputs":[{"name":"a","type":"address"}],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"},{"type":"function","name":"apy","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]"#;

    #[test]
    fn dedups_identical_calls() {
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\ncontracts:\n  - name: c\n    metric_prefix: c\n    abi_inline: '{ABI}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n      - function: apy\n      - name: apy_again\n        function: apy\n"
        );
        let (plan, _r) = build_from(&yaml);
        assert_eq!(plan.calls.len(), 1);
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].metric_name, "c_apy");
    }

    #[test]
    fn label_union_backfills_missing_keys() {
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\ncontracts:\n  - name: c\n    metric_prefix: c\n    abi_inline: '{ABI}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n        labels:\n          pool: alpha\n      - address: \"0x0000000000000000000000000000000000000002\"\n    metrics:\n      - function: apy\n"
        );
        let (plan, _r) = build_from(&yaml);
        assert_eq!(plan.entries.len(), 2);
        for e in &plan.entries {
            assert_eq!(e.gauge_label_keys(), vec!["pool", "address", "chain_id"]);
        }
    }

    #[test]
    fn chunk_count_rounds_up() {
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\nscrape:\n  max_calls_per_batch: 2\ncontracts:\n  - name: c\n    metric_prefix: c\n    abi_inline: '{ABI}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n      - function: balanceOf\n        calls:\n          - args: [\"0x0000000000000000000000000000000000000010\"]\n          - args: [\"0x0000000000000000000000000000000000000011\"]\n          - args: [\"0x0000000000000000000000000000000000000012\"]\n"
        );
        let (plan, _r) = build_from(&yaml);
        assert_eq!(plan.calls.len(), 3);
        assert_eq!(plan.chunk_count, 2);
    }
}
