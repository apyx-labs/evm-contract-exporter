use std::collections::BTreeMap;

use anyhow::{Context, Result};
use prometheus::{CounterVec, Gauge, GaugeVec, Opts, Registry};

/// Exporter self-observability metrics (Go spec §8). Top-level config labels
/// are applied as const labels on every series.
pub struct SelfMetrics {
    pub scrape_duration_seconds: Gauge,
    pub scrape_errors_total: CounterVec,
    pub call_errors_total: CounterVec,
    pub last_scrape_success_timestamp: Gauge,
    pub rpc_block_number: Gauge,
    pub calls_total: Gauge,
    pub chunks_total: Gauge,
    pub build_info: GaugeVec,
}

impl SelfMetrics {
    pub fn new(registry: &Registry, top_level_labels: &BTreeMap<String, String>) -> Result<Self> {
        let const_labels: std::collections::HashMap<String, String> = top_level_labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let g = |name: &str, help: &str| -> Result<Gauge> {
            Gauge::with_opts(Opts::new(name, help).const_labels(const_labels.clone()))
                .with_context(|| format!("create {name}"))
        };

        let scrape_duration_seconds = g(
            "evm_exporter_scrape_duration_seconds",
            "Wall-clock time of the most recent scrape attempt, in seconds.",
        )?;
        let scrape_errors_total = CounterVec::new(
            Opts::new(
                "evm_exporter_scrape_errors_total",
                "Total scrape-level failures (rpc_error, block_not_available, chunk_failed, timeout, ...).",
            )
            .const_labels(const_labels.clone()),
            &["reason"],
        )?;
        let call_errors_total = CounterVec::new(
            Opts::new(
                "evm_exporter_call_errors_total",
                "Per-call decode or revert failures.",
            )
            .const_labels(const_labels.clone()),
            &["contract", "function", "address"],
        )?;
        let last_scrape_success_timestamp = g(
            "evm_exporter_last_scrape_success_timestamp_seconds",
            "Unix timestamp of the last fully successful scrape.",
        )?;
        let rpc_block_number = g(
            "evm_exporter_rpc_block_number",
            "Block number used at the most recent scrape.",
        )?;
        let calls_total = g(
            "evm_exporter_calls_total",
            "Number of view-function calls planned per scrape.",
        )?;
        let chunks_total = g(
            "evm_exporter_chunks_total",
            "Number of aggregate3 invocations per scrape (calls / max_calls_per_batch, rounded up).",
        )?;
        let build_info = GaugeVec::new(
            Opts::new(
                "evm_exporter_build_info",
                "Exporter build information. Constant 1.",
            )
            .const_labels(const_labels.clone()),
            &["version", "commit", "rust_version"],
        )?;

        registry.register(Box::new(scrape_duration_seconds.clone()))?;
        registry.register(Box::new(scrape_errors_total.clone()))?;
        registry.register(Box::new(call_errors_total.clone()))?;
        registry.register(Box::new(last_scrape_success_timestamp.clone()))?;
        registry.register(Box::new(rpc_block_number.clone()))?;
        registry.register(Box::new(calls_total.clone()))?;
        registry.register(Box::new(chunks_total.clone()))?;
        registry.register(Box::new(build_info.clone()))?;

        Ok(Self {
            scrape_duration_seconds,
            scrape_errors_total,
            call_errors_total,
            last_scrape_success_timestamp,
            rpc_block_number,
            calls_total,
            chunks_total,
            build_info,
        })
    }

    pub fn set_build_info(&self, version: &str, commit: &str, rust_version: &str) {
        let version = if version.is_empty() { "dev" } else { version };
        let commit = if commit.is_empty() { "unknown" } else { commit };
        let rust_version = if rust_version.is_empty() {
            "unknown"
        } else {
            rust_version
        };
        self.build_info
            .with_label_values(&[version, commit, rust_version])
            .set(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_all_self_metrics() {
        let reg = Registry::new();
        let labels = BTreeMap::from([("chain".to_string(), "ethereum".to_string())]);
        let m = SelfMetrics::new(&reg, &labels).expect("self metrics");
        m.set_build_info("1.2.3", "abc", "1.80");
        // CounterVecs only appear in gather() once a label set is observed
        // (matches Go client_golang behaviour), so touch each one.
        m.scrape_errors_total.with_label_values(&["timeout"]).inc();
        m.call_errors_total
            .with_label_values(&["c", "f", "0xabc"])
            .inc();
        let families = reg.gather();
        let names: Vec<&str> = families.iter().map(|f| f.name()).collect();
        for want in [
            "evm_exporter_scrape_duration_seconds",
            "evm_exporter_scrape_errors_total",
            "evm_exporter_call_errors_total",
            "evm_exporter_last_scrape_success_timestamp_seconds",
            "evm_exporter_rpc_block_number",
            "evm_exporter_calls_total",
            "evm_exporter_chunks_total",
            "evm_exporter_build_info",
        ] {
            assert!(names.contains(&want), "missing {want}");
        }
    }

    #[test]
    fn rejects_double_register() {
        let reg = Registry::new();
        let labels = BTreeMap::new();
        SelfMetrics::new(&reg, &labels).expect("first");
        assert!(SelfMetrics::new(&reg, &labels).is_err());
    }
}
