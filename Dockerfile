# Build stage
FROM rust:1-slim AS builder

ARG GIT_COMMIT=""
ENV GIT_COMMIT=${GIT_COMMIT}

RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# RUSTC_VERSION is surfaced on evm_exporter_build_info{rust_version=...}.
RUN RUSTC_VERSION="$(rustc --version)" cargo build --release --locked \
    && cp target/release/evm-contract-exporter /evm-contract-exporter

# Final stage — minimal, non-root.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /evm-contract-exporter /usr/local/bin/evm-contract-exporter

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/evm-contract-exporter"]
