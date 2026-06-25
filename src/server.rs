use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use prometheus::{Registry, TextEncoder};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ServerConfig {
    pub listen_address: String,
    pub metrics_path: String,
    pub health_path: String,
}

pub struct Server {
    cfg: ServerConfig,
    registry: Arc<Registry>,
}

impl Server {
    pub fn new(cfg: ServerConfig, registry: Arc<Registry>) -> Self {
        Self { cfg, registry }
    }

    pub fn router(&self) -> Router {
        let metrics_path = if self.cfg.metrics_path.is_empty() {
            "/metrics".to_string()
        } else {
            self.cfg.metrics_path.clone()
        };
        let health_path = if self.cfg.health_path.is_empty() {
            "/healthz".to_string()
        } else {
            self.cfg.health_path.clone()
        };
        Router::new()
            .route(&metrics_path, get(metrics_handler))
            .route(&health_path, get(health_handler))
            .with_state(self.registry.clone())
    }

    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.cfg.listen_address)
            .await
            .with_context(|| format!("bind {}", self.cfg.listen_address))?;
        axum::serve(listener, self.router())
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .context("http server")?;
        Ok(())
    }
}

async fn metrics_handler(State(registry): State<Arc<Registry>>) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    match encoder.encode_to_string(&registry.gather()) {
        Ok(body) => (
            StatusCode::OK,
            [("Content-Type", "text/plain; version=0.0.4")],
            body,
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("Content-Type", "text/plain")],
            format!("encode error: {e}"),
        ),
    }
}

async fn health_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", "text/plain; charset=utf-8")],
        "ok",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn server() -> Server {
        let reg = Registry::new();
        let g = prometheus::Gauge::new("test_gauge", "t").expect("g");
        reg.register(Box::new(g.clone())).expect("reg");
        g.set(42.0);
        Server::new(
            ServerConfig {
                listen_address: "127.0.0.1:0".into(),
                metrics_path: "/metrics".into(),
                health_path: "/healthz".into(),
            },
            Arc::new(reg),
        )
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_text() {
        let resp = server()
            .router()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(String::from_utf8_lossy(&bytes).contains("test_gauge 42"));
    }

    #[tokio::test]
    async fn health_endpoint_ok() {
        let resp = server()
            .router()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
