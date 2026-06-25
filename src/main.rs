use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use evm_contract_exporter::config;
use evm_contract_exporter::exporter::{Build, Exporter};

/// Build identity injected at compile time (wired by the Dockerfile/CI).
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = match option_env!("GIT_COMMIT") {
    Some(c) => c,
    None => "unknown",
};

#[derive(Parser, Debug)]
#[command(name = "evm-contract-exporter", version)]
struct Cli {
    /// Path to the exporter YAML config (required).
    #[arg(long)]
    config: PathBuf,
    /// Log level: trace, debug, info, warn, error.
    #[arg(long, default_value = "info")]
    log_level: String,
    /// Log format: json or text.
    #[arg(long, default_value = "json")]
    log_format: String,
    /// Load config, validate against RPC (chain_id + block_tag probes), exit 0.
    #[arg(long, default_value_t = false)]
    validate_only: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("evm-contract-exporter: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    init_logging(&cli.log_level, &cli.log_format)?;

    let cfg = config::load(&cli.config)
        .with_context(|| format!("config load failed: {}", cli.config.display()))?;
    tracing::info!(
        path = %cli.config.display(),
        chain_id = cfg.chain.chain_id,
        contracts = cfg.contracts.len(),
        "config loaded"
    );

    let listen = cfg.server.listen_address.clone();
    let metrics_path = cfg.server.metrics_path.clone();
    let block_tag = cfg.chain.block_tag.clone();
    let interval = cfg.scrape.interval;

    let exporter = Exporter::new(cfg).await.context("exporter init failed")?;
    exporter.set_build_info(&Build {
        version: VERSION.to_string(),
        commit: COMMIT.to_string(),
        rust_version: option_env!("RUSTC_VERSION")
            .unwrap_or("unknown")
            .to_string(),
    });

    if cli.validate_only {
        tracing::info!(path = %cli.config.display(), "config is valid");
        return Ok(());
    }

    tracing::info!(
        listen_address = %listen,
        metrics_path = %metrics_path,
        block_tag = %block_tag,
        scrape_interval_ms = interval.as_millis() as u64,
        version = VERSION,
        commit = COMMIT,
        "starting exporter"
    );

    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());
    Arc::new(exporter).run(cancel).await
}

fn init_logging(level: &str, format: &str) -> Result<()> {
    let filter =
        EnvFilter::try_new(level).map_err(|_| anyhow::anyhow!("invalid log level: {level:?}"))?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        "json" => builder.json().init(),
        "text" => builder.init(),
        other => bail!("invalid log format: {other:?} (expected json or text)"),
    }
    Ok(())
}

fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    });
}
