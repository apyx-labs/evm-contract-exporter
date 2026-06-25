pub mod abi;
pub mod args;
pub mod naming;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use alloy_dyn_abi::{DynSolType, Specifier};
use alloy_json_abi::JsonAbi;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::abi::{output_scalar_is_numeric, resolve_function, resolve_tuple_field};
use crate::config::naming::infer_metric_names;

pub const DEFAULT_MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
pub const DEFAULT_BLOCK_TAG: &str = "finalized";
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_SCRAPE_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_SCRAPE_TIMEOUT: Duration = Duration::from_secs(25);
pub const DEFAULT_MAX_CALLS_PER_BATCH: usize = 500;
pub const DEFAULT_LISTEN_ADDRESS: &str = "0.0.0.0:9100";
pub const DEFAULT_METRICS_PATH: &str = "/metrics";
pub const DEFAULT_HEALTH_PATH: &str = "/healthz";

pub const RESERVED_LABEL_KEYS: [&str; 2] = ["address", "chain_id"];

pub type Labels = BTreeMap<String, String>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub chain: Chain,
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub scrape: Scrape,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub contracts: Vec<Contract>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    #[serde(default)]
    pub rpc_url: String,
    #[serde(default)]
    pub chain_id: u64,
    #[serde(default)]
    pub multicall3_address: String,
    #[serde(default)]
    pub block_tag: String,
    #[serde(default, with = "humantime_serde")]
    pub request_timeout: Duration,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default)]
    pub listen_address: String,
    #[serde(default)]
    pub metrics_path: String,
    #[serde(default)]
    pub health_path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scrape {
    #[serde(default, with = "humantime_serde")]
    pub interval: Duration,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub max_calls_per_batch: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub name: String,
    #[serde(default)]
    pub metric_prefix: String,
    #[serde(default)]
    pub abi_path: String,
    #[serde(default)]
    pub abi_inline: String,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(skip)]
    pub parsed_abi: Option<JsonAbi>,
}

impl Contract {
    /// Returns `metric_prefix` if set, else `name` (Go `EffectiveMetricPrefix`).
    pub fn effective_metric_prefix(&self) -> &str {
        if self.metric_prefix.is_empty() {
            &self.name
        } else {
            &self.metric_prefix
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub address: String,
    #[serde(default)]
    pub labels: Labels,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub function: String,
    #[serde(default)]
    pub args: Vec<serde_yaml_ng::Value>,
    #[serde(default)]
    pub outputs: Vec<Output>,
    #[serde(default)]
    pub calls: Vec<Call>,
    #[serde(default)]
    pub labels: Labels,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Output {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub scale: f64,
}

impl Output {
    /// Returns `scale` if set, else 1.0 (mirrors Go `EffectiveScale`).
    pub fn effective_scale(&self) -> f64 {
        if self.scale == 0.0 { 1.0 } else { self.scale }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    #[serde(default)]
    pub args: Vec<serde_yaml_ng::Value>,
    #[serde(default)]
    pub labels: Labels,
}

/// Reads a YAML file, expands `${ENV}` in every string leaf, applies defaults,
/// and runs offline validation. Mirrors Go `config.Load`.
pub fn load(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read config: {}", path.display()))?;
    let mut cfg = parse_str(&raw)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    cfg.validate(base_dir)?;
    Ok(cfg)
}

/// YAML-only portion of `load`: parse -> expand env -> typed decode -> defaults.
pub fn parse_str(raw: &str) -> Result<Config> {
    let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(raw).context("parse yaml")?;
    expand_env_value(&mut value);
    let mut cfg: Config = serde_yaml_ng::from_value(value).context("decode config")?;
    cfg.apply_defaults();
    Ok(cfg)
}

impl Config {
    fn apply_defaults(&mut self) {
        if self.chain.multicall3_address.is_empty() {
            self.chain.multicall3_address = DEFAULT_MULTICALL3_ADDRESS.to_string();
        }
        if self.chain.block_tag.is_empty() {
            self.chain.block_tag = DEFAULT_BLOCK_TAG.to_string();
        }
        if self.chain.request_timeout.is_zero() {
            self.chain.request_timeout = DEFAULT_REQUEST_TIMEOUT;
        }
        if self.scrape.interval.is_zero() {
            self.scrape.interval = DEFAULT_SCRAPE_INTERVAL;
        }
        if self.scrape.timeout.is_zero() {
            self.scrape.timeout = DEFAULT_SCRAPE_TIMEOUT;
        }
        if self.scrape.max_calls_per_batch == 0 {
            self.scrape.max_calls_per_batch = DEFAULT_MAX_CALLS_PER_BATCH;
        }
        if self.server.listen_address.is_empty() {
            self.server.listen_address = DEFAULT_LISTEN_ADDRESS.to_string();
        }
        if self.server.metrics_path.is_empty() {
            self.server.metrics_path = DEFAULT_METRICS_PATH.to_string();
        }
        if self.server.health_path.is_empty() {
            self.server.health_path = DEFAULT_HEALTH_PATH.to_string();
        }
    }

    /// Runs all offline checks (Go rules 1-7). `base_dir` resolves relative
    /// `abi_path` entries; RPC-dependent rules (8, 9) live in the exporter.
    pub fn validate(&mut self, base_dir: &Path) -> Result<()> {
        if self.chain.rpc_url.is_empty() {
            bail!("chain.rpc_url is required");
        }
        if self.chain.chain_id == 0 {
            bail!("chain.chain_id is required and must be non-zero");
        }
        if self.contracts.is_empty() {
            bail!("at least one contract must be configured");
        }
        check_reserved_labels("labels", &self.labels)?;

        for i in 0..self.contracts.len() {
            let name = self.contracts[i].name.clone();
            self.validate_contract(i, base_dir)
                .with_context(|| format!("contracts[{i}] ({name})"))?;
        }
        Ok(())
    }

    fn validate_contract(&mut self, i: usize, base_dir: &Path) -> Result<()> {
        let ct = &self.contracts[i];
        if ct.name.is_empty() {
            bail!("name is required");
        }
        check_reserved_labels("labels", &ct.labels)?;
        match (ct.abi_path.is_empty(), ct.abi_inline.is_empty()) {
            (true, true) => bail!("one of abi_path or abi_inline is required"),
            (false, false) => bail!("abi_path and abi_inline are mutually exclusive"),
            _ => {}
        }
        let parsed = load_abi(ct, base_dir)?;

        if ct.instances.is_empty() {
            bail!("at least one instance is required");
        }
        for (j, inst) in ct.instances.iter().enumerate() {
            if inst.address.is_empty() {
                bail!("instances[{j}]: address is required");
            }
            check_reserved_labels(&format!("instances[{j}].labels"), &inst.labels)?;
        }
        if ct.metrics.is_empty() {
            bail!("at least one metric is required");
        }
        for (mi, m) in ct.metrics.iter().enumerate() {
            validate_metric(&parsed, m)
                .with_context(|| format!("metrics[{mi}] ({})", metric_descriptor(m)))?;
        }
        self.contracts[i].parsed_abi = Some(parsed);
        Ok(())
    }
}

/// envVarRE matches `${NAME}` where NAME is a valid shell identifier.
/// Replaces `${NAME}` with the env var value; leaves bare `$NAME` untouched.
pub fn expand_env(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(close) = s[i + 2..].find('}')
        {
            let name = &s[i + 2..i + 2 + close];
            if is_shell_ident(name) {
                out.push_str(&std::env::var(name).unwrap_or_default());
                i = i + 2 + close + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_shell_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn expand_env_value(v: &mut serde_yaml_ng::Value) {
    use serde_yaml_ng::Value;
    match v {
        Value::String(s) => *s = expand_env(s),
        Value::Sequence(seq) => seq.iter_mut().for_each(expand_env_value),
        Value::Mapping(map) => map.iter_mut().for_each(|(_, val)| expand_env_value(val)),
        _ => {}
    }
}

fn is_valid_prom_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c == ':' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == ':' || c.is_ascii_alphanumeric())
}

const PROM_NAME_RE_NOTE: &str = "Prometheus metric name grammar: ^[a-zA-Z_:][a-zA-Z0-9_:]*$";

fn check_reserved_labels(ctx: &str, labels: &Labels) -> Result<()> {
    for k in labels.keys() {
        if RESERVED_LABEL_KEYS.contains(&k.as_str()) {
            bail!("{ctx}: label {k:?} is reserved and set by the exporter");
        }
    }
    Ok(())
}

fn load_abi(ct: &Contract, base_dir: &Path) -> Result<JsonAbi> {
    let raw = if !ct.abi_inline.is_empty() {
        ct.abi_inline.clone()
    } else {
        let p = Path::new(&ct.abi_path);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        };
        std::fs::read_to_string(&full)
            .with_context(|| format!("read abi_path {:?}", ct.abi_path))?
    };
    serde_json::from_str::<JsonAbi>(&raw).context("parse abi")
}

pub(crate) fn metric_descriptor(m: &Metric) -> String {
    if !m.name.is_empty() {
        m.name.clone()
    } else if !m.function.is_empty() {
        m.function.clone()
    } else if !m.calls.is_empty() {
        "calls[...]".into()
    } else {
        "<unnamed>".into()
    }
}

fn validate_metric(abi: &JsonAbi, m: &Metric) -> Result<()> {
    check_reserved_labels("labels", &m.labels)?;
    if m.function.is_empty() {
        bail!("function is required");
    }
    if !m.args.is_empty() && !m.calls.is_empty() {
        bail!("args and calls are mutually exclusive");
    }
    let func = resolve_function(abi, &m.function)?;
    validate_outputs(func, &m.outputs)?;

    if !m.calls.is_empty() {
        for (i, call) in m.calls.iter().enumerate() {
            check_reserved_labels(&format!("calls[{i}].labels"), &call.labels)?;
            if call.args.len() != func.inputs.len() {
                bail!(
                    "calls[{i}]: function {:?} expects {} argument(s), got {}",
                    func.name,
                    func.inputs.len(),
                    call.args.len()
                );
            }
        }
    } else if m.args.len() != func.inputs.len() {
        bail!(
            "function {:?} expects {} argument(s), got {}",
            func.name,
            func.inputs.len(),
            m.args.len()
        );
    }

    if !m.name.is_empty() && !is_valid_prom_name(&m.name) {
        bail!(
            "metric name {:?} does not match {PROM_NAME_RE_NOTE}",
            m.name
        );
    }
    for name in infer_metric_names(m, func)? {
        if !is_valid_prom_name(&name) {
            bail!("inferred metric name {name:?} does not match {PROM_NAME_RE_NOTE}");
        }
    }
    Ok(())
}

fn validate_outputs(func: &alloy_json_abi::Function, outputs: &[Output]) -> Result<()> {
    let n = func.outputs.len();
    let mut seen: std::collections::HashSet<(usize, String)> = std::collections::HashSet::new();
    for (i, o) in outputs.iter().enumerate() {
        if o.index >= n {
            bail!(
                "outputs[{i}].index {} out of range (function {:?} has {n} output(s))",
                o.index,
                func.name
            );
        }
        if !seen.insert((o.index, o.field.clone())) {
            if !o.field.is_empty() {
                bail!(
                    "outputs[{i}]: duplicate index {} field {:?}",
                    o.index,
                    o.field
                );
            }
            bail!("outputs[{i}]: duplicate index {}", o.index);
        }
        if !o.field.is_empty() {
            let (fi, _) = resolve_tuple_field(func, o.index, &o.field)?;
            let param = &func.outputs[o.index];
            let component = &param.components[fi];
            let ty: DynSolType = component
                .resolve()
                .with_context(|| format!("resolve tuple field type for {:?}", o.field))?;
            if !output_scalar_is_numeric(&ty) {
                bail!(
                    "outputs[{i}]: tuple field {:?} has type {}, not a numeric/bool scalar",
                    o.field,
                    component.ty
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod load_tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tmp");
        f.write_all(contents.as_bytes()).expect("write");
        f
    }

    const MINIMAL: &str = r#"
chain:
  rpc_url: ${TEST_RPC_URL}
  chain_id: 1
contracts:
  - name: c
    abi_inline: '[{"type":"function","name":"apy","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]'
    instances:
      - address: "0x0000000000000000000000000000000000000001"
    metrics:
      - function: apy
"#;

    #[test]
    fn applies_defaults() {
        unsafe { std::env::set_var("TEST_RPC_URL", "http://localhost:8545") };
        let f = write_tmp(MINIMAL);
        let cfg = load(f.path()).expect("load");
        assert_eq!(cfg.chain.multicall3_address, DEFAULT_MULTICALL3_ADDRESS);
        assert_eq!(cfg.chain.block_tag, "finalized");
        assert_eq!(
            cfg.chain.request_timeout,
            std::time::Duration::from_secs(10)
        );
        assert_eq!(cfg.scrape.interval, std::time::Duration::from_secs(30));
        assert_eq!(cfg.scrape.timeout, std::time::Duration::from_secs(25));
        assert_eq!(cfg.scrape.max_calls_per_batch, 500);
        assert_eq!(cfg.server.listen_address, "0.0.0.0:9100");
        assert_eq!(cfg.server.metrics_path, "/metrics");
        assert_eq!(cfg.server.health_path, "/healthz");
        assert_eq!(cfg.chain.rpc_url, "http://localhost:8545");
    }

    #[test]
    fn rejects_unknown_fields() {
        unsafe { std::env::set_var("TEST_RPC_URL", "http://localhost:8545") };
        let bad = MINIMAL.replace("chain_id: 1", "chain_id: 1\n  bogus: true");
        let f = write_tmp(&bad);
        assert!(load(f.path()).is_err());
    }

    #[test]
    fn env_expands_braced_only() {
        unsafe { std::env::set_var("FOO", "bar") };
        assert_eq!(expand_env("${FOO}/x"), "bar/x");
        assert_eq!(expand_env("$FOO/x"), "$FOO/x");
        assert_eq!(expand_env("${MISSING_VAR_XYZ}"), "");
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use std::path::Path;

    fn cfg_from(yaml: &str) -> Config {
        unsafe { std::env::set_var("TEST_RPC_URL", "http://localhost:8545") };
        parse_str(yaml).expect("parse")
    }

    const VIEW_ABI: &str = r#"[{"type":"function","name":"apy","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"},{"type":"function","name":"balanceOf","inputs":[{"name":"a","type":"address"}],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]"#;

    fn base(metrics: &str, extra_contract: &str) -> String {
        format!(
            "chain:\n  rpc_url: ${{TEST_RPC_URL}}\n  chain_id: 1\ncontracts:\n  - name: c\n    abi_inline: '{VIEW_ABI}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n{metrics}{extra_contract}"
        )
    }

    #[test]
    fn valid_config_passes() {
        let mut c = cfg_from(&base("      - function: apy\n", ""));
        assert!(c.validate(Path::new(".")).is_ok());
        assert!(c.contracts[0].parsed_abi.is_some());
    }

    #[test]
    fn rejects_missing_rpc_url() {
        let mut c = parse_str("chain:\n  chain_id: 1\ncontracts: []\n").expect("parse");
        assert!(c.validate(Path::new(".")).is_err());
    }

    #[test]
    fn rejects_zero_chain_id() {
        let mut c = parse_str("chain:\n  rpc_url: x\ncontracts: []\n").expect("parse");
        let err = c.validate(Path::new(".")).expect_err("should fail");
        assert!(err.to_string().contains("chain_id"));
    }

    #[test]
    fn rejects_non_view_function() {
        let abi = r#"[{"type":"function","name":"poke","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"nonpayable"}]"#;
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\ncontracts:\n  - name: c\n    abi_inline: '{abi}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n      - function: poke\n"
        );
        let mut c = parse_str(&yaml).expect("parse");
        assert!(c.validate(Path::new(".")).is_err());
    }

    #[test]
    fn rejects_arg_count_mismatch() {
        let mut c = cfg_from(&base("      - function: balanceOf\n", ""));
        assert!(c.validate(Path::new(".")).is_err());
    }

    #[test]
    fn rejects_reserved_label() {
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\nlabels:\n  address: nope\ncontracts:\n  - name: c\n    abi_inline: '{VIEW_ABI}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n      - function: apy\n"
        );
        let mut c = parse_str(&yaml).expect("parse");
        let err = c.validate(Path::new(".")).expect_err("fail");
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn rejects_args_and_calls_together() {
        let abi = r#"[{"type":"function","name":"balanceOf","inputs":[{"name":"a","type":"address"}],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]"#;
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\ncontracts:\n  - name: c\n    abi_inline: '{abi}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n      - function: balanceOf\n        args: [\"0x0000000000000000000000000000000000000002\"]\n        calls:\n          - args: [\"0x0000000000000000000000000000000000000003\"]\n"
        );
        let mut c = parse_str(&yaml).expect("parse");
        assert!(c.validate(Path::new(".")).is_err());
    }
}
