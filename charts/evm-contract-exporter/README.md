# evm-contract-exporter Helm chart

Deploys the EVM Contract Prometheus Exporter — a per-chain sidecar that
batch-reads view/pure functions from configured EVM smart contracts via
Multicall3 and exposes their return values as Prometheus gauges.

See the repository root [`README.md`](../../README.md) for the full metric
model and config schema.

## Quickstart

1. Create a Secret holding the chain RPC URL:

   ```bash
   kubectl -n monitoring create secret generic evm-contract-exporter-rpc \
     --from-literal=ETH_RPC_URL='https://eth.example.com/v3/<api-key>'
   ```

2. Install with the example values file:

   ```bash
   helm install evm-contract-exporter charts/evm-contract-exporter/ \
     -n monitoring --create-namespace \
     --values charts/evm-contract-exporter/examples/chainlink-eth-usd.yaml \
     --set image.tag=<sha>
   ```

3. Port-forward and sanity-check metrics:

   ```bash
   kubectl -n monitoring port-forward svc/evm-contract-exporter 9100:9100
   curl -s localhost:9100/metrics | grep '^chainlink_eth_usd_'
   curl -s localhost:9100/metrics | grep '^evm_exporter_'
   ```

## Required inputs

| Value | Purpose |
|---|---|
| `image.tag` | Image tag (commit SHA published by CI). |
| `rpcUrlSecret.name` / `rpcUrlSecret.key` | Existing Kubernetes Secret + key whose value is the chain RPC URL. Mounted as `ETH_RPC_URL` in the container. |
| `config` | Exporter YAML config — rendered verbatim into the ConfigMap at `/etc/evm-contract-exporter/config.yaml`. Schema is in the repo root README. |
| `abis` | Map of `filename -> raw ABI JSON string`. Each entry is mounted as `/etc/evm-contract-exporter/<filename>` alongside the config. Reference these files from the config via `abi_path`. |

## How ABI files are shipped

The root Dockerfile produces a minimal image containing only the compiled
binary — it does **not** ship any ABI tree. That's deliberate: which ABIs an
operator needs depends on which contracts they're scraping, and baking every
known ABI into the image bloats it for everyone.

Instead, this chart mounts ABI JSON directly from the same ConfigMap that
holds `config.yaml`, via the `abis:` values map. Each key becomes a filename
under `/etc/evm-contract-exporter/`; the exporter's config references them
using absolute paths:

```yaml
# values.yaml
abis:
  AggregatorV3.json: |-
    [ { "type": "function", "name": "latestRoundData", ... } ]

config:
  contracts:
    - name: chainlink_eth_usd
      abi_path: /etc/evm-contract-exporter/AggregatorV3.json
      # ...
```

Tradeoff: ABIs are versioned with the chart release (good — no "mystery
files" floating in the image, clean security review, clean rollback). Cost:
operators have to paste/pipe ABIs into values.

## Validating config changes in CI

The exporter supports `--validate-only` for config validation:

```bash
export ETH_RPC_URL=https://eth.llamarpc.com
cargo run -- \
  --config charts/evm-contract-exporter/examples/chainlink-eth-usd.yaml \
  --validate-only
```

Exit code `0` means the config parses, ABIs resolve, function names and
arg types check out, the chain's `eth_chainId` matches `chain.chain_id`,
and `eth_getBlockByNumber(block_tag)` returns a real block.

## Prometheus scrape config

The pod sets the standard `prometheus.io/{scrape,port,path}` annotations,
so a cluster-wide Prometheus annotation scraper picks it up automatically.

If you run the Prometheus operator, add a ServiceMonitor pointing at
`app.kubernetes.io/name=evm-contract-exporter`:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: evm-contract-exporter
  namespace: monitoring
spec:
  selector:
    matchLabels:
      app.kubernetes.io/name: evm-contract-exporter
  endpoints:
    - port: http
      path: /metrics
      interval: 30s
```

## Runbook

### `evm_exporter_scrape_errors_total` is rising

This counter increments on **scrape-wide** failures — the exporter couldn't
complete a batch at all. `reason` labels:

| `reason` | Cause | First move |
|---|---|---|
| `rpc_error` | RPC call errored or timed out | Check RPC provider status / rate limits; check `ETH_RPC_URL` secret |
| `block_not_available` | `eth_getBlockByNumber(block_tag)` returned null | Provider may not expose the configured `block_tag` (often `finalized` lags on archive-only endpoints); try `safe` or `latest` |
| `chunk_failed` | An `aggregate3` call failed mid-scrape | Usually transient RPC flake; sustained failures mean the batch is too large (lower `scrape.max_calls_per_batch`) or Multicall3 isn't at the configured address for this chain |
| `timeout` | Scrape exceeded `scrape.timeout` | Increase `scrape.timeout` or lower `scrape.max_calls_per_batch`; check RPC latency |

All gauges retain their last-known values across scrape failures — staleness
over gaps is the deliberate choice. Watch
`evm_exporter_last_scrape_success_timestamp_seconds` alongside this counter.
If it's older than a few intervals, alert.

### `evm_exporter_call_errors_total` is non-zero

Per-call failures — one specific view function reverted or returned
un-decodable data. Labels `contract`, `function`, `address` pin the culprit.

Common causes:
- The contract's view function reverts under specific state (e.g. an oracle
  read reverts on stale data). The scrape itself succeeds; just that gauge
  doesn't update.
- An ABI mismatch (regenerate or re-paste the ABI; re-run `--validate-only`).
- The instance address is wrong or the contract was removed.

`call_errors_total` rising without matching `scrape_errors_total` movement
means the exporter is healthy but a specific call is broken — investigate
that contract/function, don't page on the exporter.

### Stale metrics / values aren't updating

1. Compare `evm_exporter_rpc_block_number` to the chain's latest block (or
   latest finalized block when `block_tag: finalized`). Large gap → RPC
   provider issue or `finalized` lag.
2. Check `evm_exporter_last_scrape_success_timestamp_seconds` — if recent,
   scrapes are succeeding, values genuinely haven't changed.
3. `kubectl logs` — every scrape emits one structured log line with
   `block_number`, `duration_ms`, `call_count`, `error_count`.

## Values reference

See `values.yaml` for the full list with inline documentation. Key knobs:

| Key | Default | Purpose |
|---|---|---|
| `replicaCount` | `1` | Keep at 1 — multiple replicas double RPC load without benefit. |
| `logLevel` | `info` | `debug` is noisy but useful for first-install debugging. |
| `logFormat` | `json` | `text` for local tailing. |
| `service.port` | `9100` | Container listens on this port; must match `config.server.listen_address`. |
| `readinessProbe` / `livenessProbe` | enabled, `/healthz` | `/healthz` returns 200 as long as the HTTP server is up. It does **not** fail on RPC errors by design — alert on `scrape_errors_total` instead. |
| `resources` | 50m/64Mi requests, 250m/128Mi limits | Sized for ~dozens of contracts; scale up for hundreds. |
