//! `ct-mock`: serve the Connect Trade mock.
//!
//! Environment: `PORT` (default 8098), `CT_MOCK_CLIENT_ID` and `CT_MOCK_CLIENT_SECRET` (the platform credentials
//! it accepts; defaults `mock-client-id` / `mock-client-secret`), `RUST_LOG`.

use ct_mock::{serve, state, MockConfig};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let mut config = MockConfig::default();
    if let Ok(v) = std::env::var("CT_MOCK_CLIENT_ID") {
        config.client_id = v;
    }
    if let Ok(v) = std::env::var("CT_MOCK_CLIENT_SECRET") {
        config.client_secret = v;
    }
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8098);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "ct-mock listening");
    let state = state(config);
    tokio::select! {
        r = serve(listener, state) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
