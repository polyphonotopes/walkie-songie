# Gating the iroh relay without shipping a secret: attestation, "ZK", and what iroh 1.0 actually gives you

*Research / feasibility report, August 2026. Question posed: instead of a `shared_token`
gate on the self-hosted `iroh-relay` at `relay.wondering.xyz`, "could we use some kind of
ZK generator that leverages a browser attestation" so that random abusers can't use the
relay but the real app (localhost in dev; `micahscopes.github.io/*` and
`polyphonotopes.github.io/*` in prod) can — without embedding a secret in the public
wasm/JS?*

**This is a research note. No code was changed.**

---

## TL;DR

- **The premise "ship a secret the client keeps hidden" is unachievable** for a
  GitHub-Pages Rust→wasm client: anything the client can send, an attacker reads out of
  the public bundle. The `shared_token` embedded in the client is therefore a *soft gate*,
  not a secret. That is a property of open-source clients, not a bug you can ZK your way
  out of.
- **"ZK generator" is marketing here, not substance.** A zero-knowledge proof proves
  *knowledge of a witness*. If the witness lives in open-source client code, everyone has
  it and everyone can produce the proof — ZK buys nothing over shipping the token. ZK only
  helps when the witness comes from somewhere the attacker *cannot* reach, i.e. a **trusted
  hardware attester** (Apple App Attest, Android Play Integrity, a WebAuthn authenticator).
- **Those attesters are not invocable by a plain desktop web app.** The one real anonymous
  authorization primitive that wraps them — **Privacy Pass / Private Access Tokens** —
  only mints on attested platforms (Safari on recent Apple OSes; some Android). Requiring
  it would lock out your own Firefox / Linux / desktop-Chrome users. That is exactly the
  objection that killed **Web Environment Integrity**.
- **iroh 1.0.3's relay *does* expose a first-class custom auth hook.** The
  `iroh_relay::server::AccessControl` trait's `on_connect(&ClientRequest) -> Access` gives
  you the client's cryptographically-proven `endpoint_id()` **and** the full HTTP
  `headers()` of the WebSocket upgrade — including **`Origin`**. This is the real lever.
- **Recommendation:** stop treating the relay token as a secret. Build a ~50-line custom
  relay binary against `iroh-relay` whose `AccessControl` (a) **Origin-allowlists** the app
  origins for browser clients and (b) **rate-limits per proven `endpoint_id`**. Ships no
  secret, needs no minting infra, closes the drive-by-abuse case. Everything fancier
  (short-lived signed tokens, Play Integrity for the native/mobile builds) plugs into the
  *same* hook later. Do not chase attestation/ZK for the web target.

---

## 1. The primitives that actually exist today

For each: **what it proves**, **who verifies**, **support**, and **is it abuse-resistant
when the client is open-source**.

| Primitive | What it proves | Who verifies | Browser / platform reach | Usable as a gate for THIS (open-source, GitHub-Pages wasm) app? |
|---|---|---|---|---|
| **Privacy Pass / Private Access Tokens** (RFC 9576 architecture, RFC 9577 HTTP scheme, RFC 9578 issuance; Cloudflare/Apple "PAT") | An *unlinkable, one-time* token that some **Attester** vouched for the client (real device / passed a CAPTCHA), redeemed at an **Origin** without the Origin learning which issuance it came from | The **Origin** (here: the relay/proxy), against an **Issuer**'s public key | PAT auto-mints in **Safari on recent Apple OSes** (App Attest–backed) and in limited Android/Play deployments. **Desktop Chrome/Firefox/Linux mint nothing.** | **No, not as a universal gate.** You'd exclude most of your own users, and you must become a registered Origin with an Issuer relationship. It *is* the honest "anonymous token" primitive, but its reach is the problem. |
| **WebAuthn / passkeys (attestation)** | That *a credential was created and the client holds its private key*; with direct attestation, the authenticator's make/model. **Not** device-integrity / non-emulation. | The **Relying Party's own server** — no third party | Broad (all modern browsers), **but requires a user gesture + a registered credential**, and consumer flows almost always use `none` attestation for privacy | **Only if you add accounts.** It authenticates *a registered user*, not "the real app". Turns an anonymous P2P app into a login-walled one. Heavy for the goal. |
| **Play Integrity (Android) / App Attest + DeviceCheck (Apple)** | That a request comes from a **genuine, unmodified app instance on a genuine device**, signed by a hardware/OS key | Your backend, against Google/Apple public keys | **Native apps only.** Irrelevant to a browser tab; **relevant to the Tauri/native + mobile builds** | **Yes — for the native/mobile clients specifically.** This is the one place a *strong* attestation is actually obtainable. Useless for the web target. |
| **Private State Tokens / Trust Tokens (Chrome)** | Trust conveyed from an **issuer** site to a **redeemer** site without cross-site tracking (blind-signature tokens; IETF Privacy Pass basis) | The redeemer site | **Chrome only**, via origin trials; needs issuer enrollment | **No.** Chrome-only, requires an issuer ecosystem you don't have; not a per-connection relay gate. |
| **Web Environment Integrity (WEI)** — *withdrawn* | Would have let a site demand an **attester** (e.g. Google Play) vouch that the *browser/device environment* is "trustworthy" | The site | Never shipped. **Abandoned 2023-11-02, repo archived 2024-12-03**; narrowed to an Android-WebView-only API | **No — and instructive.** It was killed precisely because "attest the environment or you're locked out" breaks the open web (alternative browsers, ad-blockers, Linux). Your use-case would hit the same wall. |

Sources: [RFC 9576 — Privacy Pass Architecture](https://datatracker.ietf.org/doc/rfc9576/)
(defines Client / Origin / Attester / Issuer and the four unlinkability goals; references
RFC 9577 HTTP auth scheme and RFC 9578 issuance);
[Private State Tokens](https://privacysandbox.google.com/protections/private-state-tokens)
(Chrome, origin-trial, IETF Privacy Pass basis);
[Web Environment Integrity repo](https://github.com/RupertBenWiser/Web-Environment-Integrity)
(archived; Google pivoted to an Android-only API) and
[Wikipedia: Web Environment Integrity](https://en.wikipedia.org/wiki/Web_Environment_Integrity)
(withdrawal timeline + open-web/DRM criticism from Mozilla, FSF);
[webauthn.guide — attestation](https://webauthn.guide/) (attestation proves credential
creation + key possession, verified at the Relying Party, `none` attestation is the
consumer default). Note: I could not fetch Cloudflare's PAT blog (404 at time of writing);
the PAT platform-support specifics above are stated from general knowledge and the RFC
role model, not from a fetched Cloudflare page — verify OS/version specifics before relying
on them.

### The one non-obvious upside in that table

**Play Integrity / App Attest is genuinely obtainable for the native/Tauri/mobile builds.**
If those builds matter, they are the *only* clients that can present a strong,
non-forgeable attestation. Gate them differently (and better) than the web client. Don't
try to make the web client reach for something only a native app can hold.

---

## 2. The ZK angle — blunt assessment

**For this use-case, "ZK" is marketing, not substance.** The reasoning:

1. **ZK proves knowledge of a witness.** The whole value is that a verifier learns "the
   prover knows `w` such that `P(w)`" without learning `w`.
2. **In an open-source client, the witness is public.** Whatever secret/circuit-input the
   wasm bundle would feed the prover is sitting in a `.wasm` on GitHub Pages. Any abuser
   extracts `w` and generates a byte-valid proof. You've rebuilt the shared-token gate with
   a cryptography bill attached.
3. **ZK only helps when the witness is unreachable to the attacker** — i.e. it originates
   in a **secure enclave / platform attester** the app merely *relays*. That is precisely
   what Privacy Pass already is: a **blind-signature anonymous credential** where the
   unlinkability (issuer can't correlate issuance to redemption) is the "zero-knowledge–
   flavored" property, and the un-forgeable root is the platform Attester. **Privacy Pass
   is the ZK-shaped answer** — you don't roll your own SNARK, you use the standardized
   anonymous token. And its limits (§1) are the real story.
4. **"zk-attested WebAuthn" / anonymous credentials from ECDSA** (Google's zk-creds,
   Cloudflare's anonymous-credential work) exist in research, but **none is a browser API a
   wasm app can call today.** There is no `navigator.proveOrigin()`.

**Connection to this repo's other ZK note.** `docs/research/zk-provable-dag-snapshots.md`
reaches the structurally identical conclusion for a different problem: there, ZK-*privacy*
is explicitly *not* the requirement (every room member sees every op) — what's needed is
"succinctness + soundness," i.e. an authenticated data structure, not a zero-knowledge
proof. Same pattern here: the honest primitive is an **unlinkable bearer token backed by a
trusted attester** (Privacy Pass), plus an **origin binding** — not a bespoke ZK circuit.
In both cases "ZK" is the label reached for; the substance is a commitment/token scheme.
The repo keeps discovering that its real needs are one notch simpler than "zero-knowledge."

**Where a ZK proof *would* legitimately earn its keep here:** essentially nowhere for the
web client. The only scenario is if you wanted an *unlinkable* attestation from the
native/mobile builds (prove "a valid Play Integrity verdict exists for me" without the
relay learning the device identity). Privacy Pass already covers that pattern off-the-shelf;
a hand-rolled zk-SNARK would be strictly more work for the same guarantee.

---

## 3. iroh 1.0's relay: the real auth extensibility (with exact API)

**Confirmed against `iroh-relay` v1.0.3** (the version pinned in `Cargo.toml` /
`Cargo.lock`) via docs.rs and the crate source on `github.com/n0-computer/iroh`.

### 3.1 There is a first-class custom authorization callback — yes.

`iroh_relay::server` exports a public trait:

```rust
pub trait AccessControl: std::fmt::Debug + Send + Sync + 'static {
    fn on_connect(&self, request: &ClientRequest) -> impl Future<Output = Access> + Send;
    fn on_disconnect(&self, endpoint_id: EndpointId, connection_id: ConnectionId) { /* … */ }
}
// plus the object-safe `DynAccessControl` (blanket-impl'd for any `AccessControl`)
// wired into the server via `RelayConfig::access: Arc<dyn DynAccessControl>`.

pub enum Access { Allow, Deny { reason: Option<String> } }
```

`on_connect` runs **once per incoming connection, before it is registered**. Return
`Access::Deny { reason }` to reject. This is exactly the "plug in a verifier" hook the
question asked for — it is not limited to `shared_token`.

### 3.2 What the callback can see — `ClientRequest` (all public in 1.0.3):

| Method | Returns | Why it matters here |
|---|---|---|
| `endpoint_id()` | `EndpointId` | **Cryptographically proven** by the relay handshake (client signs TLS-exported keying material / a server challenge). A stable, un-spoofable per-client Ed25519 identity **even without any token** → per-identity rate-limit / ban for free. |
| `headers()` | `&http::HeaderMap` | **The full HTTP headers of the WS upgrade — including `Origin`.** This is the lever for an origin-allowlist. |
| `auth_token()` | `Option<String>` | `Authorization: Bearer …`, falling back to the **`?token=` query param** (browsers can't set WS auth headers, so this fallback is how a browser passes a token). |
| `query_pairs()`, `uri()`, `protocol_version()`, `connection_id()` | — | Additional context. |

### 3.3 The built-in config knobs (stock binary) and their ceiling.

The shipped `iroh-relay` binary's config `access` field is an enum:
`everyone` (`AllowAll`) · `allowlist` (by `EndpointId`) · `denylist` · `http` · `shared_token`.

Two important limitations of the *stock* binary:

- **`shared_token`** matches `auth_token()` against a configured list (overridable by the
  `IROH_RELAY_ACCESS_TOKEN` env var). This is the current gate — and it's the one that
  can't stay secret in a web client.
- **`http`** (delegate to an external auth service) **forwards only the proven
  `X-Iroh-Endpoint-Id`** to your endpoint (plus an optional bearer that authenticates the
  *relay* to *your* service). It does **not** forward the client's `Origin` or the client's
  own token. So the stock `http` mode **cannot** make an Origin- or PAT-based decision —
  it only knows the endpoint id, which is useless for gating unknown browser users.

**Therefore: to gate on `Origin` or on an attestation/PAT token, you must run a custom
`AccessControl` impl** — i.e. a small binary built against the `iroh-relay` *library*
(`RelayConfig { access: Arc::new(MyAccess), .. }`), not the stock binary's config file.
Since the relay is self-hosted and this is a Rust shop, that's ~dozens of lines, not a
fork.

Sources: [docs.rs iroh-relay 1.0.3 `server`](https://docs.rs/iroh-relay/1.0.3/iroh_relay/server/index.html)
(confirms `AccessControl`, `DynAccessControl`, `ClientRequest`, `Access`, `AllowAll`);
[docs.rs `ClientRequest`](https://docs.rs/iroh-relay/1.0.3/iroh_relay/server/struct.ClientRequest.html)
(confirms `headers()`, `endpoint_id()`, `auth_token()`, `query_pairs()`, `uri()`,
`protocol_version()`, `connection_id()`); crate source
`iroh-relay/src/main.rs` and `iroh-relay/src/server.rs` (the `AccessConfig` enum and the
`http` mode forwarding only the endpoint id).

---

## 4. Is `Origin` spoofable? (the crux for options b/c/e)

**From inside a browser: no.** `Origin` is a
[Forbidden request header](https://developer.mozilla.org/docs/Glossary/Forbidden_header_name):
the browser sets it automatically on the WebSocket handshake to the page's real origin, and
page JavaScript **cannot** override or forge it. So a WS from `micahscopes.github.io` truly
carries `Origin: https://micahscopes.github.io`, and a hostile *web page* can't impersonate
that origin to your relay.

**From a non-browser client: fully spoofable.** `curl`, a headless Rust iroh client, or a
custom bot sets `Origin` to whatever it likes. So an origin allowlist is **a real barrier
to browser-based and drive-by abuse, and zero barrier to a determined non-browser
attacker.** That's the honest ceiling — and it's fine, because (a) it ships no secret, and
(b) the proven `endpoint_id` + per-IP limits cap what any single non-browser abuser can do
anyway. Treat Origin as a *filter*, never a *security boundary*.

---

## 5. The pragmatic ladder (effort × abuse-resistance)

| # | Option | Ships a secret? | Effort | Abuse-resistance | Notes |
|---|---|---|---|---|---|
| a | **Keep `shared_token`**, accept it's soft | Yes (public) | 0 | Very low | Blocks only those who won't read the source. *Footgun:* looks like security, isn't. |
| b1 | **Origin allowlist at traefik** (router `Header`/`HeadersRegexp` matcher on the WS upgrade) | No | Low (config only) | Low–med | No Rust. Blocks browser abuse + scanners; Origin spoofable by non-browser clients (§4). |
| b2 | **Origin allowlist in a custom relay `AccessControl`** (`request.headers()["origin"]`) | No | Low–med (~50-line binary) | Low–med | Same spoofability as b1, **but** combinable with per-`endpoint_id` rate-limit + `Deny{reason}`. Preferred over b1. |
| c | **Privacy Pass / PAT verification** at proxy/relay | No | High | High *for the sliver of users who have it* | Locks out Firefox/Linux/desktop-Chrome (§1). Needs Origin+Issuer enrollment. **Not viable as a universal web gate.** |
| d | **Custom relay `AccessControl` verifying a platform attestation token** | No | Med–high | **High — for native/mobile only** | The hook exists (§3). But there's no attestation a *desktop browser* can produce, so it's the right mechanism with no token to verify on web. Use it for **Tauri/Android/iOS builds** with Play Integrity / App Attest. |
| e | **Rotating short-lived tokens** from a tiny signed minter (e.g. a Cloudflare Worker), passed via `?token=`, verified (sig+exp+audience) in a custom `AccessControl` | No static secret | Med (minter + relay verify) | Med | The minter still needs a gate to decide *who* to mint for — realistically Origin+CORS+IP-rate-limit, so it inherits Origin's spoofability. **Real win:** a leaked token dies in ~60 s, and you get central rate-limit / kill-switch **without redeploying the wasm**. Good *upgrade* from b2 if abuse actually appears. |

### Recommendation

**Do not build a ZK/attestation gate for the web client.** There is no attestation a
GitHub-Pages wasm app can obtain that a non-browser abuser can't sidestep, and the one
standardized anonymous token (Privacy Pass/PAT) would exclude most of your own users —
the WEI lesson. Reframe the problem: the relay is **semi-open signaling infrastructure**;
defend it with **origin filtering + rate-limiting**, not a secret.

**Single best next step (option b2 + per-identity rate-limit):** write a small custom relay
binary against the `iroh-relay` library whose `AccessControl::on_connect`:

1. reads `request.headers()` and **allows** only when `Origin` ∈ { `http://localhost:*`
   (dev), `https://micahscopes.github.io`, `https://polyphonotopes.github.io` }; browser
   clients get in with **no secret shipped**;
2. **rate-limits / bans per `request.endpoint_id()`** (cryptographically proven, free) and
   couples that with traefik-level per-IP limits;
3. returns `Access::Deny { reason }` otherwise.

This closes the "random person points their iroh at your relay" case for ~all real abusers,
ships nothing secret, and needs no new infrastructure. It is also the *foundation* for
everything else: option **e** (short-lived signed tokens via `?token=`) and option **d**
(Play Integrity / App Attest for the native/mobile builds) both plug into the **same
`on_connect` hook** when — and only when — real abuse justifies the extra moving parts.

**When to escalate:** if Origin+rate-limit proves insufficient, add **e** (short-lived
minted tokens) for the web client and **d** (platform attestation) for the native/mobile
clients. Reserve Privacy Pass/PAT (**c**) for never, unless your user base becomes
Safari/Apple-dominant.
