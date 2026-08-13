# vance-accounts

The Vance accounts service — the marketplace's source of truth for **accounts,
sessions/devices, and (later) entitlements**. Implements the design in
[`docs/marketplace-spec.md`](../../docs/marketplace-spec.md) §4–§6, milestone
**M1**.

It is the **OAuth client** for every provider (spec §4.1), so the desktop app
never holds a provider secret. The desktop app does one uniform loopback-PKCE
flow to *this* service.

## Quick start (zero setup)

```bash
cd server/accounts
cargo run
# → vance-accounts listening on http://127.0.0.1:8787
```

With nothing configured, the built-in **`dev` provider** simulates OAuth locally
(no external services), so the whole flow — sign in → session → refresh rotation
→ device management → remote sign-out — is testable offline. This is what the
desktop client points at during development.

```bash
cargo test        # 3 unit + 4 end-to-end tests
```

## Testing the marketplace pipeline (M2) locally

There's no catalog (M3) or payments (M4) yet, so a helper publishes a `.wasm` as a
paid effect and grants it to the dev user (the same user the app's "Local dev
sign-in" uses):

```bash
# with the server running, and a built effect .wasm:
./scripts/dev-effects.sh publish path/to/effect.wasm com.you.glow "Glow"
./scripts/dev-effects.sh list
./scripts/dev-effects.sh revoke com.you.glow
```

Then in the app: Settings → **My Effects** → Install. The effect appears under
Add-on effects and applies to clips. `revoke` uninstalls it and the load-time gate
refuses to run it. **The effect's manifest `id` must equal the `<sku>`** (that's
how the gate maps them; the real catalog enforces this at publish).

## What's implemented (M1)

- **Sign-in** — loopback-PKCE handshake: `GET /auth/start` → provider →
  `GET /auth/callback` → 302 back to the app's `127.0.0.1` loopback with a
  one-time code → `POST /auth/token` (code + verifier) → `{access, refresh}`.
- **Tokens** — Ed25519-signed access tokens (15-min TTL, verified offline via
  `GET /auth/pubkey`); opaque refresh tokens, **only the sha256 is stored**.
- **Refresh rotation** with a 30-second **overlap window** (survives a
  crash/race between "server rotated" and "client persisted") and **reuse
  detection** (a replayed, rotated-away token revokes the session). Spec §4.3.
- **Password** (Argon2id) — first set needs only a session; **change requires
  step-up**. Spec §4.2.
- **Step-up ("sudo")** — `POST /v1/stepup` returns a single-use, action- and
  target-scoped token required by sensitive actions. Spec §4.4.
- **Devices** — `GET /v1/devices` (Manage Devices), `POST /v1/devices/:id/revoke`
  (remote sign-out, step-up gated). A machine is one stable device (client UUID,
  not a hardware fingerprint). Spec §5.
- **Revocation delivery** — SSE push (`GET /v1/events`), poll (`POST /v1/heartbeat`),
  and the 401 backstop on every protected call. Spec §5.

## Configuration

All via env; every value has a local-dev default (see `.env.example`). SQLite
for dev; the SQL is portable toward the Postgres prod target (spec §12).

## Real OAuth providers  ⟵ externally gated (you provide credentials)

All four exchanges are implemented (**GitHub, Google, Meta, Apple**). A provider is
active only when its client id + secret are set; until then it returns a clear
"not configured" error rather than a fake login. For every provider, register the
redirect URI **`{VANCE_ACCOUNTS_PUBLIC_URL}/auth/callback?provider=<name>`** (in
dev: `http://127.0.0.1:8787/auth/callback?provider=github`, etc.).

| Provider | Where to create the app | Set |
|---|---|---|
| **GitHub** | Settings → Developer settings → OAuth Apps → New | `VANCE_GITHUB_CLIENT_ID`, `VANCE_GITHUB_CLIENT_SECRET` |
| **Google** | Cloud Console → APIs & Services → Credentials → OAuth client ID → **Web application** | `VANCE_GOOGLE_CLIENT_ID`, `VANCE_GOOGLE_CLIENT_SECRET` |
| **Meta** | developers.facebook.com → Create App → **Facebook Login** → add Valid OAuth Redirect URI | `VANCE_META_CLIENT_ID`, `VANCE_META_CLIENT_SECRET` |
| **Apple** | developer.apple.com → Certificates, IDs & Profiles → Identifiers → **Services ID** (+ a Sign-in key `.p8`) | `VANCE_APPLE_CLIENT_ID` (the Services ID), `VANCE_APPLE_CLIENT_SECRET` |

**Apple caveat (honest):** Apple's "client secret" is itself an **ES256-signed JWT**
you generate from your Sign-in key `.p8` (with your Team ID + Key ID), valid **≤6
months**. Generate it out-of-band and put the whole JWT in
`VANCE_APPLE_CLIENT_SECRET`; **it must be regenerated before it expires**.
Auto-minting the secret from the `.p8` in-process is a planned enhancement. Also
note Apple usually returns a **private-relay email** and sends the email only on
first consent — the account is keyed on the stable `sub`, never the email (§4.1).

**Signature verification:** Google/Apple `id_token`s are verified against the
provider **JWKS (RS256)** — issuer, audience (your client id), and expiry are all
checked — not merely trusted for arriving over TLS.

## Not yet (later milestones)

Postgres driver, entitlements + activation (M2/M4), Stripe Connect webhooks (M4),
and JWKS-signature verification of Google's `id_token` (defense-in-depth hardening).
