# walkie-relay

An **Origin-gated** [iroh](https://iroh.computer) relay for walkie-songie.

The stock `iroh-relay` binary can only gate access by node allowlist/denylist or
a shared token. A browser can't hold a secret (the wasm ships publicly) and can't
set WebSocket request headers, so the only token channel from a browser is a
`?token=` query param — which we don't want. Instead, this ~200-line binary uses
`iroh-relay` as a library with a custom `AccessControl` that admits clients by
their HTTP **`Origin`**:

- browser connections whose `Origin` is allowlisted (loopback for dev + the
  configured production app origins) → **allowed**;
- connections with **no** `Origin` (native/Tauri clients, the plugin) → **allowed**;
- anything else → **denied**.

`Origin` can't be forged from *inside* a browser, so this stops other web pages
from abusing the relay. It is a filter, not a hard boundary (a non-browser client
can omit/spoof `Origin`), so it's paired with iroh-relay's per-connection
byte-rate limits. See `../docs/research/zk-relay-attestation.md` for the stronger
escalation path (short-lived signed tokens for web, Play Integrity / App Attest
for native).

## Configuration (environment)

| Var                      | Default                                                        | Meaning |
|--------------------------|----------------------------------------------------------------|---------|
| `RELAY_HTTP_BIND_ADDR`   | `0.0.0.0:3340`                                                 | Bind address (TLS terminated upstream by traefik). |
| `RELAY_ALLOWED_ORIGINS`  | `https://micahscopes.github.io,https://polyphonotopes.github.io` | Comma-separated exact origins. Loopback is always allowed on top. |
| `RELAY_RX_BYTES_PER_SEC` | `2097152`                                                     | Per-connection read rate; `0` disables. |
| `RELAY_RX_BURST_BYTES`   | `8388608`                                                     | Per-connection read burst; `0` disables burst. |
| `RUST_LOG`               | `info`                                                        | Tracing filter. |

To add another deployment origin (e.g. a new Pages site), append it to
`RELAY_ALLOWED_ORIGINS` and restart — no rebuild needed.

## Deploy (wondering.xyz)

The relay runs as the `iroh-relay` service in `~/www/docker-compose.yml`, behind
traefik. This crate replaces the old download-a-release Dockerfile with a
build-from-source one. The compose service must drop the `--config-path` command
and the `config.toml` mount (config is now env-driven) and set
`RELAY_ALLOWED_ORIGINS`. See the deploy notes in the walkie-songie session /
`docs` for the exact compose diff.

```sh
cargo build --release   # local sanity check
cargo test              # allowlist matching unit tests
```
