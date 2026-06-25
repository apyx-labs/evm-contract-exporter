use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::BlockNumberOrTag;
use anyhow::{Context, Result, anyhow, bail};
use prometheus::Registry;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::decoder;
use crate::metrics::SelfMetrics;
use crate::multicall::{CallDriver, Multicall3Driver};
use crate::planner::{self, Plan};
use crate::server::{Server, ServerConfig};

#[derive(Debug, Clone, Default)]
pub struct Build {
    pub version: String,
    pub commit: String,
    pub rust_version: String,
}

/// Narrow RPC surface used by the exporter (Go `RPCClient`).
#[allow(async_fn_in_trait)]
pub trait RpcClient: Send + Sync {
    async fn chain_id(&self) -> Result<u64>;
    /// Resolves a configured block_tag to a concrete block number.
    async fn block_number_for_tag(&self, tag: &str) -> Result<u64>;
}

pub struct Exporter<R: RpcClient, D: CallDriver> {
    cfg: Config,
    plan: Plan,
    driver: D,
    rpc: R,
    self_metrics: SelfMetrics,
    server: Server,
    registry: Arc<Registry>,
    precision_loss_seen: Mutex<HashSet<String>>,
}

impl<R: RpcClient + 'static, D: CallDriver + 'static> Exporter<R, D> {
    /// Test/seam constructor: caller supplies rpc + driver; runs the two live
    /// probes (Go rules 8 & 9) then builds the plan/metrics/server.
    pub async fn new_with_deps(cfg: Config, rpc: R, driver: D) -> Result<Self> {
        // Rule 8: chain id.
        let got = rpc.chain_id().await.context("eth_chainId probe")?;
        if got != cfg.chain.chain_id {
            bail!(
                "chain_id mismatch: config={}, rpc={}",
                cfg.chain.chain_id,
                got
            );
        }
        // Rule 9: block tag resolves.
        rpc.block_number_for_tag(&cfg.chain.block_tag)
            .await
            .with_context(|| format!("probe block_tag={:?}", cfg.chain.block_tag))?;

        let registry = Arc::new(Registry::new());
        let plan = planner::build(&cfg, &registry).context("planner.build")?;
        let self_metrics = SelfMetrics::new(&registry, &cfg.labels).context("self metrics")?;
        let server = Server::new(
            ServerConfig {
                listen_address: cfg.server.listen_address.clone(),
                metrics_path: cfg.server.metrics_path.clone(),
                health_path: cfg.server.health_path.clone(),
            },
            registry.clone(),
        );
        Ok(Self {
            cfg,
            plan,
            driver,
            rpc,
            self_metrics,
            server,
            registry,
            precision_loss_seen: Mutex::new(HashSet::new()),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn set_build_info(&self, build: &Build) {
        self.self_metrics
            .set_build_info(&build.version, &build.commit, &build.rust_version);
    }

    /// Starts the HTTP server and the scrape loop; blocks until `cancel`.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> Result<()> {
        let server_self = self.clone();
        let server_cancel = cancel.clone();
        let server_task = tokio::spawn(async move { server_self.server.run(server_cancel).await });

        // Initial synchronous scrape.
        self.run_scrape().await;

        let mut ticker = tokio::time::interval(self.cfg.scrape.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // consume the immediate first tick (initial scrape already ran)

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => { self.run_scrape().await; }
            }
        }
        match server_task.await {
            Ok(res) => res,
            Err(e) => Err(anyhow!("server task join: {e}")),
        }
    }

    async fn run_scrape(&self) {
        let timeout = self.cfg.scrape.timeout;
        match tokio::time::timeout(timeout, self.scrape_once(timeout)).await {
            Ok(Ok(())) => {}
            Ok(Err(_e)) => { /* scrape_once logs + increments its own counters */ }
            Err(_elapsed) => {
                self.self_metrics
                    .scrape_errors_total
                    .with_label_values(&["timeout"])
                    .inc();
            }
        }
    }

    /// One scrape cycle. `_timeout` mirrors the Go signature; the actual bound
    /// is applied by `run_scrape`'s `tokio::time::timeout`.
    pub async fn scrape_once(&self, _timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();

        let block_number = match self
            .rpc
            .block_number_for_tag(&self.cfg.chain.block_tag)
            .await
        {
            Ok(n) => n,
            Err(e) => {
                self.self_metrics
                    .scrape_errors_total
                    .with_label_values(&["block_not_available"])
                    .inc();
                tracing::error!(block_tag = %self.cfg.chain.block_tag, error = %e, "resolve block tag");
                self.self_metrics
                    .scrape_duration_seconds
                    .set(start.elapsed().as_secs_f64());
                return Err(e);
            }
        };

        let results = match self.driver.call(block_number, &self.plan.calls).await {
            Ok(r) => r,
            Err(e) => {
                self.self_metrics
                    .scrape_errors_total
                    .with_label_values(&["chunk_failed"])
                    .inc();
                tracing::error!(block_number, call_count = self.plan.calls.len(), error = %e, "multicall chunk failed");
                self.self_metrics
                    .scrape_duration_seconds
                    .set(start.elapsed().as_secs_f64());
                return Err(e);
            }
        };
        if results.len() != self.plan.calls.len() {
            self.self_metrics
                .scrape_errors_total
                .with_label_values(&["chunk_failed"])
                .inc();
            self.self_metrics
                .scrape_duration_seconds
                .set(start.elapsed().as_secs_f64());
            bail!(
                "multicall returned {} results, expected {}",
                results.len(),
                self.plan.calls.len()
            );
        }

        let mut err_count = 0u64;
        for entry in &self.plan.entries {
            let r = &results[entry.call_index];
            let addr_label = format!("{:#x}", entry.address);
            if !r.success {
                self.self_metrics
                    .call_errors_total
                    .with_label_values(&[&entry.contract_name, &entry.function_name, &addr_label])
                    .inc();
                tracing::debug!(contract = %entry.contract_name, function = %entry.function_name, address = %addr_label, "call reverted");
                err_count += 1;
                continue;
            }
            match decoder::decode(r.return_data.as_ref(), &entry.decode) {
                Ok((value, precision_loss)) => {
                    let label_refs: Vec<&str> =
                        entry.label_values.iter().map(String::as_str).collect();
                    entry.gauge.with_label_values(&label_refs).set(value);
                    if precision_loss {
                        self.warn_precision_loss_once(entry, &addr_label);
                    }
                }
                Err(e) => {
                    self.self_metrics
                        .call_errors_total
                        .with_label_values(&[
                            &entry.contract_name,
                            &entry.function_name,
                            &addr_label,
                        ])
                        .inc();
                    tracing::warn!(contract = %entry.contract_name, function = %entry.function_name, address = %addr_label, error = %e, "decode failed");
                    err_count += 1;
                }
            }
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.self_metrics.last_scrape_success_timestamp.set(now);
        self.self_metrics.rpc_block_number.set(block_number as f64);
        self.self_metrics
            .calls_total
            .set(self.plan.calls.len() as f64);
        self.self_metrics
            .chunks_total
            .set(self.plan.chunk_count as f64);
        self.self_metrics
            .scrape_duration_seconds
            .set(start.elapsed().as_secs_f64());

        tracing::info!(
            chain_id = self.cfg.chain.chain_id,
            block_tag = %self.cfg.chain.block_tag,
            block_number,
            duration_ms = start.elapsed().as_millis() as u64,
            call_count = self.plan.calls.len(),
            chunk_count = self.plan.chunk_count,
            error_count = err_count,
            "scrape complete"
        );
        Ok(())
    }

    fn warn_precision_loss_once(&self, entry: &planner::Entry, addr_label: &str) {
        let key = format!("{}:{}", entry.metric_name, addr_label);
        let mut seen = self
            .precision_loss_seen
            .lock()
            .expect("precision-loss mutex");
        if !seen.insert(key) {
            return;
        }
        tracing::warn!(
            metric = %entry.metric_name,
            contract = %entry.contract_name,
            function = %entry.function_name,
            address = %addr_label,
            "precision loss converting integer to float64"
        );
    }
}

/// Concrete RPC client over an alloy provider.
pub struct AlloyRpc<P: Provider> {
    provider: P,
}

impl<P: Provider> RpcClient for AlloyRpc<P> {
    async fn chain_id(&self) -> Result<u64> {
        Ok(self.provider.get_chain_id().await?)
    }

    async fn block_number_for_tag(&self, tag: &str) -> Result<u64> {
        let tag = match tag {
            "" | "latest" => BlockNumberOrTag::Latest,
            "finalized" => BlockNumberOrTag::Finalized,
            "safe" => BlockNumberOrTag::Safe,
            other => bail!("unsupported block_tag {other:?}"),
        };
        let block = self
            .provider
            .get_block_by_number(tag)
            .await?
            .ok_or_else(|| anyhow!("probe block_tag={tag:?}: rpc returned no block"))?;
        Ok(block.header.number)
    }
}

impl
    Exporter<
        AlloyRpc<alloy::providers::DynProvider>,
        Multicall3Driver<alloy::providers::DynProvider>,
    >
{
    /// Production constructor: dials the RPC, wires the multicall driver, runs
    /// probes, and builds the plan/server. Mirrors Go `exporter.New`.
    pub async fn new(cfg: Config) -> Result<Self> {
        let provider = ProviderBuilder::new()
            .connect_http(cfg.chain.rpc_url.parse().context("parse rpc_url")?)
            .erased();

        let multicall_addr: Address = cfg
            .chain
            .multicall3_address
            .parse()
            .context("parse multicall3_address")?;
        let driver = Multicall3Driver::new(
            provider.clone(),
            multicall_addr,
            cfg.scrape.max_calls_per_batch,
        )?;
        let rpc = AlloyRpc { provider };

        Self::new_with_deps(cfg, rpc, driver).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_str;
    use crate::multicall::{CallRequest, CallResult};
    use alloy::primitives::Bytes;
    use alloy::sol_types::SolValue;
    use std::path::Path;

    struct FakeRpc {
        chain_id: u64,
        block: u64,
    }
    impl RpcClient for FakeRpc {
        async fn chain_id(&self) -> Result<u64> {
            Ok(self.chain_id)
        }
        async fn block_number_for_tag(&self, _tag: &str) -> Result<u64> {
            Ok(self.block)
        }
    }

    struct FakeDriver {
        results: Vec<CallResult>,
    }
    impl CallDriver for FakeDriver {
        async fn call(&self, _block: u64, reqs: &[CallRequest]) -> Result<Vec<CallResult>> {
            assert_eq!(reqs.len(), self.results.len());
            Ok(self.results.clone())
        }
    }

    fn cfg() -> Config {
        unsafe { std::env::set_var("TEST_RPC_URL", "http://localhost:8545") };
        let abi = r#"[{"type":"function","name":"apy","inputs":[],"outputs":[{"name":"","type":"uint256"}],"stateMutability":"view"}]"#;
        let yaml = format!(
            "chain:\n  rpc_url: x\n  chain_id: 1\ncontracts:\n  - name: c\n    metric_prefix: c\n    abi_inline: '{abi}'\n    instances:\n      - address: \"0x0000000000000000000000000000000000000001\"\n    metrics:\n      - function: apy\n        outputs:\n          - index: 0\n            scale: 1e18\n"
        );
        let mut c = parse_str(&yaml).expect("parse");
        c.validate(Path::new(".")).expect("validate");
        c
    }

    #[tokio::test]
    async fn scrape_sets_gauge_value() {
        let cfg = cfg();
        let value = alloy::primitives::U256::from(50_000_000_000_000_000u64).abi_encode();
        let exp = Exporter::new_with_deps(
            cfg,
            FakeRpc {
                chain_id: 1,
                block: 100,
            },
            FakeDriver {
                results: vec![CallResult {
                    success: true,
                    return_data: Bytes::from(value),
                }],
            },
        )
        .await
        .expect("exporter");

        exp.scrape_once(Duration::from_secs(5))
            .await
            .expect("scrape");

        let families = exp.registry().gather();
        let apy = families
            .iter()
            .find(|f| f.name() == "c_apy")
            .expect("c_apy family");
        let v = apy.get_metric()[0].get_gauge().value();
        assert!((v - 0.05).abs() < 1e-12);
        let block = families
            .iter()
            .find(|f| f.name() == "evm_exporter_rpc_block_number")
            .expect("block");
        assert_eq!(block.get_metric()[0].get_gauge().value(), 100.0);
    }

    #[tokio::test]
    async fn chain_id_mismatch_fails_new() {
        let cfg = cfg();
        let res = Exporter::new_with_deps(
            cfg,
            FakeRpc {
                chain_id: 999,
                block: 1,
            },
            FakeDriver { results: vec![] },
        )
        .await;
        assert!(res.is_err());
    }
}
