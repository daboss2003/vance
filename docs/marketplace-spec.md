# Vance Effects Marketplace — Specification

Status: **design spec** (nothing here is built yet). This is the plan for turning
today's "drag-in / import a `.wasm` effect" into a full marketplace where people
**browse, buy, sell, and create** add-on effects, with accounts, devices,
payouts, and protection for paid effects.

It is deliberately honest about what is and isn't achievable — especially around
DRM and offline revocation — because building on wrong assumptions there is
expensive. Where something is a legal/finance/vendor dependency (not code), it's
flagged as **[gated]**.

---

## 1. Goals & non-goals

**Goals**
- **Browse & install** effects in-app from a hosted catalog (free and paid).
- **Buy once, use on any of your machines** — install on a new machine, or have
  it auto-install in the background (a per-device setting).
- **Sell** effects: creators get a **wallet** of earnings and can **withdraw**.
- **Accounts** via OAuth (Google / Apple / GitHub / Meta) **plus a password**
  (required to authorize a purchase and to remove the account/effects from a
  machine).
- **Manage your devices** — see where you're signed in, sign a machine out
  remotely; removing a machine **uninstalls its paid effects**.
- **Protect paid effects** so they can't just be copied and used for free, and
  can be **revoked**.
- **Let non-programmers create effects** and share/sell them.

**Non-goals (v1) / honest limits**
- **Unbreakable DRM is impossible** for a plugin that must run on the buyer's
  machine (see §7). We protect against the honest majority + casual copying, make
  leaks *traceable*, and keep the legit experience frictionless. We do **not**
  pretend a determined reverse-engineer can be stopped.
- **"Any effect, no code"** is not promised. Parameter shaders and node graphs
  cover most *visual* effects with no code; procedural/temporal/ML effects
  (tracking, optical flow) still need code and go through a hosted build tier.
- **Instant revocation of an offline machine** is not possible — offline
  licensing has an inherent grace window (§6, §7).

---

## 2. Where this builds on what exists today

Reuse, don't reinvent:

- **Manifest + pinned download.** `crates/vance-app/src/tools.rs` already streams
  a download, hashes as it goes, verifies a `sha256` pin, and atomically places
  the file (`download_to_file`, `place_artifact`, `safe_dest`). The marketplace
  install path is the same — the pin is just computed over the (possibly
  encrypted) artifact. Manifest shape mirrors `docs/tools-manifest.example.json`.
- **Add-on effect plumbing.** `add_wasm_plugin` / `remove_wasm_plugin` /
  `plugins_folder` (backend), the `effectsMarket` store + `EffectsMarketplace.svelte`
  modal (frontend) are already in place. The catalog "Browse" tab drops into the
  slot the modal already reserves.
- **The single load choke point.** All effects load through
  `vance-plugin-sdk` `host.rs` → `WasmPlugin::load(bytes)` (discovery in
  `discovery.rs`). Entitlement checks and paid-effect decryption go **right in
  front of that one call** — nowhere else.
- **The keychain milestone.** `settings.rs` already notes secrets "belong in the
  OS keychain (a later milestone)". This spec *is* that milestone.

---

## 3. Architecture at a glance

```
Desktop app (Tauri/Rust + Svelte)
  ├─ Accounts + sessions (OAuth via backend, tokens in OS keychain)
  ├─ Marketplace UI (browse / item page / My Effects / Wallet / Settings)
  ├─ Install pipeline (reuses tools.rs manifest download + sha256 pin)
  └─ Load gate in host.rs (entitlement check + paid-effect decrypt)

Backend services (vance cloud)  [new]
  ├─ accounts.vance   — OAuth client for all 4 providers, password, sessions, devices
  ├─ store.vance      — catalog: effects, versions, categories, reviews, prices
  ├─ entitle.vance    — orders → entitlements → signed entitlement tokens + license leases
  ├─ build.vance      — hosted build/sign service for the creator tiers (later phase)
  └─ payouts          — Stripe Connect (seller wallets + withdrawals)

Third parties  [gated]
  ├─ Merchant-of-Record (Paddle or Lemon Squeezy) — buyer checkout + global tax
  ├─ Stripe Connect (Express) — seller KYC + payouts
  └─ (optional) Keygen — licensing/activation/leases/revocation, self-hostable
```

Backend is the **source of truth**. The client only ever holds short-lived,
signed tokens it can verify offline.

---

## 4. Accounts & auth

### 4.1 One uniform flow (why the backend must be the OAuth client)
Do **not** attempt four native desktop OAuth integrations. Provider realities
force a hosted flow:
- **GitHub** OAuth Apps issue a `client_secret` and don't do PKCE — the secret
  can't ship in the app; the code exchange must be server-side.
- **Meta/Facebook** has no blessed desktop loopback flow and gates login behind
  app review — needs a hosted `https` redirect.
- **Apple** returns a stable `sub` but often a **private-relay/hidden email**,
  and email only on first consent.

So: `accounts.vance` is the OAuth client for all providers; the desktop does one
uniform **loopback-PKCE** flow to *our* backend (mirrors `tauri-plugin-oauth`):

1. Client generates a PKCE verifier and (first run) a random `device_id` UUID
   stored in the keychain; starts a `127.0.0.1:<ephemeral>` listener.
2. Opens the **system browser** to
   `https://accounts.vance/auth/start?provider=google&code_challenge=…&device_id=…&device_name=<hostname>&platform=macos-aarch64`.
3. Backend runs the provider handshake, upserts `user` + `oauth_identity`,
   creates a `session`, 302s back to the loopback with a one-time code.
4. Client exchanges code (+ verifier) at `/auth/token` → `{access, refresh}`.

**Account keying:** always `(provider, sub)`, **never email**. Never auto-merge
two providers by email (that's an account-takeover vector) — linking a new
provider to an existing account is an explicit step behind verified email + the
password.

### 4.2 Password (required, in addition to OAuth)
- `password_hash` (Argon2id, server-side) can be null after OAuth signup.
- The app **blocks the first purchase and any device removal** behind a
  "Create your account password" screen. This satisfies the requirement that the
  password gates purchase and remote-remove. Email-based reset flow included.

### 4.3 Tokens & storage
- **Access token**: Ed25519 JWT, 15-min TTL, claims `{sub, sid, did, ent_ver}`.
- **Refresh token**: opaque 256-bit, ~60-day sliding, **rotates every refresh**;
  server stores only `sha256(refresh)` + a generation counter.
- **Storage**: Rust `keyring` crate → OS keychain (macOS Keychain, Windows
  Credential Manager/DPAPI, Linux Secret Service). Keys: `vance.access`,
  `vance.refresh`, `vance.device_id`, `vance.entitlement_token`. **Avoid Tauri
  Stronghold** (deprecated, removed in v3).

> **[limitation — close in-milestone] Linux keychain may be absent** (headless /
> minimal / some tiling-WM boxes have no Secret Service daemon). Ship a fallback:
> an app-managed encrypted file keyed by a machine-derived secret, with graceful
> handling of a locked/denied keychain — or legit Linux users get locked out.

> **[trap] Refresh rotation → false-positive lockouts.** If the app is killed
> after the server rotates but before the client persists the new refresh, or two
> refreshes race (window + background auto-install), naive reuse-detection nukes
> the whole session. Mitigate: single-flight refresh through one mutex; a short
> rotation **overlap window** (accept the immediately-previous generation for
> ~10–30 s); persist the new refresh **before** using the new access token.

### 4.4 Step-up ("sudo") for sensitive actions
Purchase, remove-device, change-password require step-up:
`POST /v1/stepup {action, target_ref, password}` → server verifies Argon2id →
returns a single-use step-up token (5-min, scoped to `{action,target_ref,sid}`)
that the sensitive endpoint requires in an `X-Step-Up` header. Optional short
"sudo grace" so a burst of purchases doesn't re-prompt.

---

## 5. Devices & sessions

- **Registration** at first `/auth/token` for a `device_id` (client-generated
  UUID in keychain — **not** a hardware fingerprint; those are brittle across
  VMs, dual-boot, hardware swaps, reimaging and cause false lockouts). `device_name`
  defaults to hostname, user-editable. Re-auth on the same machine **reuses** the
  session row, so a machine is one stable device.
- **Manage Devices** screen: `GET /v1/devices` → list `{name, platform,
  app_version, last_seen, this_device}`; per-row **Sign out** → `POST
  /v1/devices/{id}/revoke` (step-up required).
- **Buy-once vs device caps** — an explicit policy call. Recommended: **no hard
  cap** (a cap contradicts "use on any machine" and creates "remove one to add
  one" friction), but track devices, show the Manage-Devices screen, and use
  server-side abuse signals (many new devices/hr, impossible-travel logins)
  rather than blocking installs. This is the JetBrains/Adobe posture.

### How a removed device learns and self-uninstalls (3 tiers)
1. **Push (SSE):** the app holds `GET /v1/events` (auth via Authorization
   header, **never** the token in the URL). On `session.revoked`, the client
   wipes keychain tokens and runs the uninstall routine.
2. **Poll/heartbeat:** every launch + ~10 min, `POST /v1/heartbeat` returns
   `ent_ver` + a revoked flag — catches machines offline at revocation time.
3. **Backstop:** any call on a revoked session returns `401`; 15-min access TTL
   means a revoked device loses API access within one TTL regardless.

**Uninstall routine:** for each entitlement-gated sku the device installed,
delete `~/.vance/plugins/<sku>.wasm` and `rescan_plugins`. Free effects are
untouched.

> **[limitation — the honest truth]** File deletion is **tidy-up UX, not the
> security boundary.** A revoked *offline* laptop never gets the push, and a
> copied `.wasm` survives deletion. The real gate is **at load** (§6): the host
> refuses to instantiate a paid effect without a valid signed entitlement token.
> An offline machine keeps working until its grace window lapses — inherent to
> offline licensing (every vendor lives with this).

> **[trap] Clock rollback** defeats naive offline expiry (set the clock back).
> Bind grace to a server timestamp captured at last heartbeat; keep a monotonic
> "last seen server time" high-water mark and refuse if the local clock is
> earlier. Raises the bar; doesn't beat a determined attacker.

---

## 6. Entitlements — "buy once, use on any machine"

- Entitlements are **user-scoped** (named-user model): any device with a live
  session may install an owned effect. That *is* "buy once."
- Server issues a **signed entitlement token** (Ed25519 JWT: `{sub, did,
  skus:[…], iat, exp=iat+14d, ent_ver}`) on login / heartbeat /
  `entitlement.changed`; cached in the keychain. `exp` is the **offline grace
  window** (buyer works ~14 days with no internet; while online it silently
  renews).
- **Enforce at load:** before instantiating a paid effect, `host.rs` checks a
  currently-valid entitlement token lists that sku. Free effects skip the check.
- **Install / auto-install:** reconcile = "for each active entitlement not on
  this device, install it." Manual (click Install) or **Auto** (Settings toggle
  `autoInstallPurchases`, per device) → a background task installs newly-entitled
  skus silently. Install reuses the manifest pipeline: `GET
  /v1/artifacts/{sku}?platform=…` returns `{url, sha256, dest}` where `url` is a
  **short-lived signed URL** minted only after the server re-checks entitlement;
  client verifies the sha256 pin (unchanged) and places the file.
- **Refunds/chargebacks** flow through the **same** `entitlement.changed` path →
  `state=refunded` → uninstall, or paid effects leak after refunds.

---

## 7. Protecting paid effects (DRM) — honest and light

### 7.1 The reality (design for it, don't fight it)
To run an effect, the host must hand **plaintext** wasm to `Module::new`. So at
some instant the decrypted module is in our process memory, and the key is on the
client. A user with local admin + a debugger can dump memory or patch the gate.
**No client-side scheme changes this.** WASM is weakly protected by design
(readable text format, decompilers). Obfuscation/white-box crypto only raise
cost; they never make it impossible, and heavy DRM (the iLok backlash) **loses
paying customers**.

**What's actually achievable (our real threat model = the 95%+ honest/casual
users):** casual "copy the file to a friend" fails; the default artifact is inert
without an entitlement; revocation works against non-crackers within the grace
window; leaks become **traceable** (watermark) → identifiable, bannable leakers.
**Not achievable:** stopping a skilled RE from making one redistributable crack.
We accept this openly.

### 7.2 Recommended scheme (light, invisible, reliable)
Prefer **buying** the licensing layer (**Keygen** — self-hostable; Ed25519-signed
+ AES-256-GCM license files, machine activation, heartbeat/lease, revocation,
device caps all first-class — or Cryptlex/LicenseSpring). We write only the
**decrypt-on-load shim**.

1. **Encrypt once per version.** Random 256-bit content key (CEK), XChaCha20-
   Poly1305 or AES-256-GCM (Rust `chacha20poly1305`/`aes-gcm`/`ring`). Ship the
   ciphertext as a normal manifest artifact; the manifest `sha256` is over the
   **ciphertext** so `tools.rs` is reused unchanged. **Don't per-user-encrypt the
   artifact** — it kills CDN caching + dedup and adds no security. Per-device
   applies to the **key**.
2. **Device identity + keychain.** First run: random install GUID + an X25519
   device keypair in the keychain. Fingerprint = `SHA-256(install GUID)` (hardware
   id only a weak secondary signal).
3. **Activate once online.** On purchase, client sends `{license, device pubkey,
   fingerprint}`; server returns a **signed lease** (Ed25519: license id, entitled
   skus, fingerprint, ~14-day expiry, buyer id for watermark, and the **signed
   sha256 of the plaintext wasm**) + the **CEK sealed to the device pubkey**
   (libsodium sealed box). Persist both in the keychain.
4. **Decrypt inside the gate (not an `if licensed {}` branch).** Verify the lease
   locally (Ed25519 against a baked-in public key + not-expired + fingerprint +
   entitled) → unseal the CEK → AEAD-decrypt in memory → verify plaintext sha256
   against the lease → hand straight to `Module::new`. **Never write plaintext to
   disk.** Because the CEK only materializes if the lease verifies, there's no
   boolean to NOP out.
5. **Offline grace + silent heartbeat.** Lease valid offline until expiry; renew
   silently while online. Past grace with no renewal → disable with a **visible**
   "reconnect to renew your license" notice (no silent fallback).
6. **Revocation.** Remove device / refund / chargeback → deactivate server-side +
   add to a revocation set. The removed device's short lease stops renewing → the
   effect dies within the grace window; if online, the heartbeat wipes the CEK +
   lease immediately.

> **[must]** Sign the **plaintext** hash (not just ciphertext) and verify it
> before `Module::new`, so a tampered ciphertext can't smuggle a different module
> past the sandbox. The decrypt shim feeds the **same** hardened `WasmPlugin::load`
> (fuel cap, 256 MB memory, manifest-size cap) — never a bypass.

### 7.3 Where to stop
Build §7.2; do **not** add heavy WASM obfuscation / anti-debug for v1 (diminishing
returns, perf/brittleness, complicates our own sandbox audit). Spend the marginal
effort on **watermarking** (buyer identity in the lease now; per-buyer artifact
diversification later) and cheap high-leverage levers: clear EULA, account bans,
revoke-on-chargeback, and above all a **frictionless legit experience** (buy =
auto-install + updates + works on every signed-in device, seamless machine moves).
Most people pay for convenience and fair pricing when DRM is invisible.

---

## 8. Payments & payouts  [gated — vendor + legal]

**Recommended "hybrid": Merchant-of-Record for money-in, Stripe Connect for
money-out.**

- **Buy side (money-in):** front the buyer with a **MoR** (Paddle or Lemon
  Squeezy). The MoR is the reseller and handles **all global VAT/GST/US sales tax
  + chargebacks**; Vance is its single "seller." Effective cost ~7–8%. **Open the
  MoR hosted checkout in the SYSTEM BROWSER** (Tauri opener), **not** the embedded
  webview — this gives card + **Apple Pay + Google Pay** + Link/PayPal
  automatically and avoids the whole class of "Apple Pay won't render in WKWebView"
  bugs. On success, the MoR webhook → `entitle.vance` issues the signed
  entitlement/lease → client installs via the manifest pipeline.
- **Sell side (wallet + withdraw):** **Stripe Connect (Express)**. Each seller
  onboards an Express account; **Stripe hosts KYC/identity/bank/tax-forms** and
  pays out to banks in 100+ countries. The "wallet balance" = an **accrued-royalty
  ledger we keep** (buyer paid MoR → we compute seller share = price − platform
  cut − processing reserve) → pay out via Connect transfers (on-demand withdrawal
  or scheduled with a minimum threshold).
- **Platform split:** take **20–30%**; sellers get ~70% (matches app-store
  expectations plugin sellers already accept; JetBrains ≤25% is a good benchmark).

> **Apple Pay ≠ IAP.** Apple Pay for digital goods on desktop *web* is allowed
> (just processor fees). The 15–30% cut only appears if you route through
> Apple/Google **IAP inside a store-distributed build**.

> **[traps]**
> - **MoRs can't do marketplace payouts** — they pay one account (you). You still
>   need Stripe Connect/PayPal/Wise for sellers. (No MoR does multi-party payout.)
> - **Pure Stripe Connect = you own the tax** (deemed marketplace facilitator;
>   register + file VAT/sales tax everywhere; Stripe Tax only *calculates*). Prefer
>   the MoR front early; switch to pure Connect only once volume funds a tax
>   function.
> - **Mac App Store trap:** selling effects in-app there forces StoreKit IAP at
>   30% (15% small-biz), incompatible with a seller wallet. **Don't ship the
>   marketplace in a MAS build** (or US-only link-out). **Microsoft Store is fine**
>   (0% on non-gaming own-commerce). Keep the marketplace in the self-distributed
>   installer.
> - **"Wallet" is a regulated word** — keep it a simple accrued-royalty ledger that
>   pays out via Stripe; never let buyers preload/store balances (money-transmitter
>   licensing risk). Money flows buyer → MoR → us → seller payout, never pooled.
> - **Onboarding is slow** (MoR approval, Stripe Connect KYC) — bake in weeks of
>   lead time; don't gate GA on it.
> - **EU DSA "trader verification"** applies even to free-with-donation sellers —
>   budget KYC + a public seller-identity surface from day one.

---

## 9. Catalog / store server

**Store = a superset of the JSON manifest** (mirror the Open VSX architecture):
Postgres (publishers, effects, versions, categories, reviews, prices, downloads)
+ object storage/CDN. Every published version still emits a **signed,
sha256-pinned** static manifest entry, so the pinned-artifact contract stays the
source of truth.

**Effect package** = a signed archive (e.g. `.vfx`) with:
- `manifest.toml` (mirrors VS Code `package.json` + Blender `manifest.toml`):
  `id` reverse-DNS (`com.publisher.glow`), semver `version`, `displayName`,
  `publisher`, `description`, **fixed** `categories` (Color, Blur/Sharpen,
  Stylize, Keying, Transitions, Generators, Distortion, Audio-reactive…),
  free-form `tags`, SPDX `license`, **`engine`** host-API range (like
  `engines.vscode`), **`capabilities`** requested (gpu, cpu-only,
  network=deny-by-default, file-read — enforced by the sandbox), `pricing`
  (free | paid + amount + model), `icon`, `screenshots[]`, `changelog`.
- Artifact: `effect.wasm` **or** (no-code tier) `shader.fs` + `params.json`
  (ISF-shaped).
- `signatures`: per-artifact sha256 (already do) + **Ed25519 signature over the
  manifest, applied by the store at publish** (mirror VS Code auto-signing).

**Endpoints:** `GET /effects?q&category&sort&page` → cards; `GET /effects/{id}` →
metadata + `versions[]` + screenshots + rating + price + capabilities; `GET
/effects/{id}/reviews`.

**Publisher verification (two tiers):** email-verified free authors; **DNS-TXT
domain-verified** brands get a badge; paid sellers also complete MoR KYC / EU DSA
trader verification.

**Reviews:** 1–5 stars + text, **gated to users who installed/purchased** (kills
most spam), aggregate on the card, velocity limits + moderation queue.

**In-app flow:** Browse → item page (screenshots, reviews, price, requested
capabilities) → Buy (MoR in system browser) → Install (download, verify sha256 +
Ed25519 against the pinned manifest, **diff requested capabilities vs sandbox
policy and prompt for any elevated capability**) → run sandboxed. Updates = version
bump + re-verify; **flag ownership transfers before auto-updating** (supply-chain
guard).

**Review pipeline:** automated gate on every publish (wasm validates + within
size budget + no disallowed host imports + capabilities match declaration +
shader compiles via `naga` on a sample frame). Manual review only for **featured +
paid** listings; free tier is automated + reactive takedown, or it won't scale.

> **[trap] Supply chain.** Real precedent: an attacker bought 30 WordPress plugins
> (~400k installs) and backdoored all of them — the gap was ownership transfer with
> no signing. Mitigate: sign at publish, keep sha256 pinning, **re-verify on every
> update**, hold/flag ownership transfers, require reproducible builds in the
> hosted tier.

---

## 10. Creator tooling — how non-programmers make effects

Presented as **tiers, not a funnel**. Parameter shaders + node graphs cover most
*visual* effects with true no-code; procedural/temporal/ML effects need code
(hosted-build tier). Don't promise "any effect, no code."

- **Phase 0 — Starter template (programmers first).** Freeze the package format,
  host ABI, and capability manifest. Ship `create-vance-effect` (cargo-generate /
  Extism-PDK style) with **one-command build** (`wasm32` + `wasm-opt`) and a local
  `vance dev` live preview on a sample clip. Validates the pipeline end-to-end.
- **Phase 1 — No-code shader tier (the real unlock).** Adopt/adapt **ISF**. In-app
  **Effect Studio**: paste/edit one GLSL fragment body + fill a params form;
  Vance validates + translates via **naga → WGSL → wgpu** (our existing render
  stack), auto-generates sliders/toggles/color-pickers from the schema,
  live-previews, and publishes through the same signing pipeline. Seed a template
  gallery (blur, color grade, glow, chroma key, transitions). Doable by
  non-programmers because they tweak values + one shader, not Rust.
- **Phase 2 — Guided node/graph builder.** A node graph over curated GLSL blocks
  that compiles to a single ISF fragment shader. Zero GLSL written.
- **Phase 3 — Hosted build/publish service.** Cloud build (bundled toolchain, no
  local install) takes source (Rust/AssemblyScript/C via Extism PDKs) → compiles →
  runs the automated checks → **signs with the store Ed25519 key** → lists. A web
  "New Effect" wizard ties it to the catalog.

---

## 11. Settings dialog (new)

Required by "install in the background depending on their settings" + device/account
management. A proper **Settings** dialog (currently only privacy opt-ins exist,
inside onboarding):
- **Account** — identity, linked providers (Google/Apple/GitHub/Meta),
  Create/Change password, sign out.
- **Devices** — list (name, platform, app version, last-seen, "this device"),
  Sign out per device (step-up), rename this device.
- **Purchases / My Effects** — owned effects, per-device Install/Uninstall state,
  `autoInstallPurchases` toggle.
- **Wallet** (sellers) — balance ledger, Stripe Connect onboarding status,
  Withdraw.
- Existing **privacy** opt-ins move here too.

---

## 12. Data model (Postgres-shaped)

```
users(id, email citext unique, email_verified, password_hash null, password_set_at, created_at, updated_at)
oauth_identities(id, user_id, provider{google,apple,github,meta}, provider_subject, email_at_provider, linked_at, UNIQUE(provider, provider_subject))
sessions(id=session_id, user_id, device_id, device_name, platform, app_version, refresh_token_hash, refresh_generation, created_at, last_seen_at, ip_last, revoked_at null, revoked_reason null, UNIQUE(user_id, device_id))
entitlements(id, user_id, sku, source{purchase,bundle,subscription,grant}, state{active,refunded,expired,revoked}, granted_at, expires_at null, order_id, UNIQUE(user_id, sku))
orders(id, user_id, sku, amount, currency, processor, processor_ref, status, step_up_id, created_at)
plugin_artifacts(sku, version, platform, object_key, sha256, dest)   -- entitlement-gated serve
device_installs(session_id, sku, installed_version, installed_at, uninstalled_at null)
step_up_challenges(id, user_id, session_id, action{purchase,remove_device,change_password}, target_ref, created_at, expires_at, consumed_at null)
-- store side:
publishers(id, user_id, display_name, verified_domain null, kyc_status)
effects(id=sku, publisher_id, display_name, description, categories[], tags[], license, pricing, icon, created_at)
effect_versions(sku, version, platform, object_key, sha256, encrypted bool, key_id null, engine_range, capabilities[], changelog, signature, published_at)
reviews(id, sku, user_id, stars, body, created_at, UNIQUE(sku, user_id))   -- gated to owners
```

---

## 13. Build order (milestones)

1. **M1 — Accounts + devices + keychain.** OAuth-via-backend, password, sessions,
   refresh rotation (with the overlap fix), OS-keychain storage **+ Linux
   fallback**, Manage-Devices screen, Settings dialog shell. *(Closes the deferred
   keychain milestone.)*
2. **M2 — Entitlements + install pipeline.** Signed entitlement tokens,
   **load-time gate** in `host.rs`, artifact serve behind signed URLs, manual +
   auto install, reconcile-on-login, uninstall-on-revoke.
3. **M3 — Catalog browse (free effects).** `store.vance`, signed catalog manifest,
   in-app Browse tab in the existing modal, capability-prompt on install, reviews.
4. **M4 — Payments (buy paid effects).** MoR checkout in system browser →
   webhook → entitlement; paid-effect **encryption + decrypt-on-load shim** (or
   adopt Keygen); refund→revoke wiring.
5. **M5 — Seller wallet + payouts.** Stripe Connect Express onboarding, royalty
   ledger, Withdraw UI; EU DSA trader verification.
6. **M6 — Creator tooling.** Phase 0 starter template → Phase 1 Effect Studio
   (ISF shaders) → Phase 2 node graph → Phase 3 hosted build.

M1–M2 are **client + backend fundamentals** and unblock everything. M3 delivers
visible value (browse/install free) without payments. Paid (M4+) is where the
[gated] vendor/legal work lands.

---

## 14. Decisions needed from you  [gated / product]

- **Merchant-of-Record:** Paddle vs Lemon Squeezy (both handle global tax +
  wallets like Apple/Google Pay via system-browser checkout).
- **Payout rail:** Stripe Connect Express (recommended) vs PayPal/Wise.
- **Platform cut:** 20% / 25% / 30%.
- **Buy licensing:** adopt **Keygen** (self-hostable, does activation/leases/
  revocation) vs build the license server ourselves.
- **Device policy:** no hard cap (recommended) vs a cap (e.g. 3 machines).
- **Offline grace window:** 7 vs 14 days (revocation speed ⇄ offline tolerance).
- **Distribution:** self-installer + Microsoft Store carry the marketplace; **Mac
  App Store build ships without in-app buying** — confirm.
- **Legal:** seller ToS granting Vance resale rights (needed for the MoR-reseller
  model), buyer EULA, EU DSA seller-identity surface.

---

## 15. Honest limitations (kept visible, per "address all limitations")

1. **Plaintext-in-memory is unavoidable** — a skilled RE with local admin can
   dump a decrypted paid effect. We protect the honest majority + make leaks
   traceable; we don't claim to stop crackers.
2. **Offline revocation isn't instant** — a revoked offline machine works until
   its grace window lapses. Inherent to offline licensing.
3. **File-delete uninstall is UX, not security** — the security boundary is the
   **load-time entitlement gate**.
4. **Linux keychain can be absent** — must ship the encrypted-file fallback in the
   same milestone.
5. **No-code ≠ every effect** — parameter/graph tiers cover most visual effects;
   procedural/temporal/ML effects need the code/hosted-build tier.
6. **Tax/compliance is real** — solved by the MoR front, not by us registering in
   100+ jurisdictions.
