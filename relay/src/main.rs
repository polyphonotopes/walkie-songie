//! walkie-relay — a thin `iroh-relay` wrapper that gates connections by browser
//! **Origin** instead of a shared token.
//!
//! The stock `iroh-relay` binary only offers everyone/allowlist/denylist/http/
//! shared_token access modes; none can admit a client by its HTTP `Origin`
//! header. This binary uses `iroh-relay` as a library with a custom
//! [`AccessControl`] that:
//!
//!   * allows browser connections whose `Origin` is in the allowlist — loopback
//!     for local dev, plus the configured production app origins;
//!   * allows connections with **no** `Origin` header — native/Tauri clients and
//!     the plugin are not browsers and never send one;
//!   * denies everything else.
//!
//! `Origin` cannot be forged from *within* a browser, so this blocks other web
//! pages from abusing the relay. It is NOT a hard boundary — a non-browser
//! client can omit or spoof `Origin` — so it is paired with iroh-relay's
//! built-in per-connection byte-rate limits. See
//! `docs/research/zk-relay-attestation.md` for the stronger (short-lived signed
//! token / native attestation) escalation path.
//!
//! TLS is terminated upstream (traefik), so this serves plain HTTP.
//!
//! ## Configuration (environment)
//!
//! | Var                      | Default                    | Meaning                                   |
//! |--------------------------|----------------------------|-------------------------------------------|
//! | `RELAY_HTTP_BIND_ADDR`   | `0.0.0.0:3340`             | Address the relay HTTP server binds.       |
//! | `RELAY_ALLOWED_ORIGINS`  | the two github.io origins | Comma-separated exact origins. Loopback is always allowed on top of these. |
//! | `RELAY_RX_BYTES_PER_SEC` | `2097152` (2 MiB/s)        | Per-connection read rate. `0` disables.    |
//! | `RELAY_RX_BURST_BYTES`   | `8388608` (8 MiB)          | Per-connection read burst. `0` disables burst. |
//! | `RUST_LOG`               | `info`                     | Standard tracing filter.                    |

use std::num::NonZeroU32;
use std::sync::Arc;

use iroh_relay::server::{
    Access, AccessControl, ClientRateLimit, ClientRequest, Limits, RelayConfig, Server,
    ServerConfig,
};

const DEFAULT_BIND_ADDR: &str = "0.0.0.0:3340";
const DEFAULT_ALLOWED_ORIGINS: &str =
    "https://micahscopes.github.io,https://polyphonotopes.github.io";
const DEFAULT_RX_BYTES_PER_SEC: &str = "2097152";
const DEFAULT_RX_BURST_BYTES: &str = "8388608";

/// Admits a client if it presents an allowlisted `Origin`, or none at all.
#[derive(Debug)]
struct OriginAllowlist {
    /// Exact production origins, lowercased (`scheme://host[:port]`).
    exact: Vec<String>,
}

impl OriginAllowlist {
    fn is_allowed(&self, origin: &str) -> bool {
        let origin = origin.trim().to_ascii_lowercase();
        if self.exact.iter().any(|allowed| *allowed == origin) {
            return true;
        }
        // Loopback dev origins: any scheme/port on a loopback host.
        matches!(host_of(&origin), Some("localhost" | "127.0.0.1" | "::1"))
    }
}

/// Extracts the host from an `Origin` value (`scheme://host[:port]`), handling
/// bracketed IPv6 literals such as `http://[::1]:8888`.
fn host_of(origin: &str) -> Option<&str> {
    let authority = origin.split_once("://")?.1;
    // An Origin has no path, but be defensive and stop at one anyway.
    let authority = authority.split('/').next().unwrap_or(authority);
    if let Some(v6) = authority.strip_prefix('[') {
        // `[::1]:8888` -> `::1`
        return v6.split(']').next();
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

impl AccessControl for OriginAllowlist {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        match request.headers().get(http::header::ORIGIN) {
            // Native/Tauri clients and the plugin are not browsers — no Origin.
            None => Access::Allow,
            Some(origin) => match origin.to_str() {
                Ok(value) if self.is_allowed(value) => Access::Allow,
                Ok(value) => {
                    tracing::debug!(origin = %value, "denied: origin not allowlisted");
                    Access::Deny {
                        reason: Some("origin not allowed".to_string()),
                    }
                }
                Err(_) => Access::Deny {
                    reason: Some("origin not valid utf-8".to_string()),
                },
            },
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bind_addr: std::net::SocketAddr = env_or("RELAY_HTTP_BIND_ADDR", DEFAULT_BIND_ADDR)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid RELAY_HTTP_BIND_ADDR: {e}"))?;

    let exact: Vec<String> = env_or("RELAY_ALLOWED_ORIGINS", DEFAULT_ALLOWED_ORIGINS)
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    tracing::info!(
        %bind_addr,
        allowed_origins = ?exact,
        "walkie-relay: Origin-gated iroh relay (loopback always allowed; no-Origin clients admitted)"
    );

    let mut relay = RelayConfig::new(bind_addr);
    relay.access = Arc::new(OriginAllowlist { exact });

    // Preserve the per-connection byte-rate limits from the previous config.
    let rx_bps = env_or("RELAY_RX_BYTES_PER_SEC", DEFAULT_RX_BYTES_PER_SEC)
        .parse::<u32>()
        .unwrap_or(0);
    if let Some(bps) = NonZeroU32::new(rx_bps) {
        let mut rate = ClientRateLimit::new(bps);
        rate.max_burst_bytes = env_or("RELAY_RX_BURST_BYTES", DEFAULT_RX_BURST_BYTES)
            .parse::<u32>()
            .ok()
            .and_then(NonZeroU32::new);
        let mut limits = Limits::default();
        limits.client_rx = Some(rate);
        relay.limits = limits;
    }

    let mut config = ServerConfig::default();
    config.relay = Some(relay);

    let mut server = Server::spawn(config)
        .await
        .map_err(|e| anyhow::anyhow!("relay failed to start: {e}"))?;
    tracing::info!(http_addr = ?server.http_addr(), "relay listening");

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => tracing::info!("shutdown signal received"),
        res = server.join() => tracing::warn!(?res, "relay task exited on its own"),
    }

    server
        .shutdown()
        .await
        .map_err(|e| anyhow::anyhow!("relay shutdown error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> OriginAllowlist {
        OriginAllowlist {
            exact: vec![
                "https://micahscopes.github.io".to_string(),
                "https://polyphonotopes.github.io".to_string(),
            ],
        }
    }

    #[test]
    fn allows_production_origins() {
        let c = control();
        assert!(c.is_allowed("https://micahscopes.github.io"));
        assert!(c.is_allowed("https://polyphonotopes.github.io"));
        // case-insensitive
        assert!(c.is_allowed("https://Polyphonotopes.GitHub.io"));
    }

    #[test]
    fn allows_loopback_any_scheme_and_port() {
        let c = control();
        assert!(c.is_allowed("http://localhost:8888"));
        assert!(c.is_allowed("http://localhost"));
        assert!(c.is_allowed("http://127.0.0.1:9999"));
        assert!(c.is_allowed("https://127.0.0.1:8443"));
        assert!(c.is_allowed("http://[::1]:8888"));
    }

    #[test]
    fn denies_other_origins() {
        let c = control();
        assert!(!c.is_allowed("https://evil.example.com"));
        // no substring/suffix confusion
        assert!(!c.is_allowed("https://micahscopes.github.io.evil.com"));
        assert!(!c.is_allowed("http://localhost.evil.com"));
        assert!(!c.is_allowed("https://notgithub.io"));
    }

    #[test]
    fn host_of_parses_forms() {
        assert_eq!(host_of("http://localhost:8888"), Some("localhost"));
        assert_eq!(
            host_of("https://micahscopes.github.io"),
            Some("micahscopes.github.io")
        );
        assert_eq!(host_of("http://[::1]:8888"), Some("::1"));
        assert_eq!(host_of("not-an-origin"), None);
    }
}
