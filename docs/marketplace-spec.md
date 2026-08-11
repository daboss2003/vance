# Vance Effects Marketplace — Specification

Status: **design spec** (nothing here is built yet). This is the plan for turning
today's "drag-in / import a `.wasm` effect" into a full marketplace where people
**browse, buy, sell, and create** add-on effects, with accounts, devices,
activation codes, and protection for paid effects — where **Vance only ever
receives money and never pays anyone out** (§8).

It is deliberately honest about what is and isn't achievable — especially around
DRM and offline revocation — because building on wrong assumptions there is
expensive. Where something is a legal/finance/vendor dependency (not code), it's
flagged as **[gated]**.

---

## 1. Goals & non-goals

**Goals**
- **Browse & install** effects in-app from a hosted catalog (free and paid).
- **Free effects: use on unlimited machines. Paid effects: up to 2 machines** at a
  time — install on a new machine, or have it auto-install in the background (a
  per-device setting).
- **Sell** effects **without Vance touching the buyer's money**: the buyer pays on
  the **seller's own connected Stripe/PayPal**, funds land in the seller's account,
  and the processor's paid-webhook **auto-grants** the effect to the buyer. Vance
  skims a **20% fee** and **only ever receives money, never sends it** (no payouts,
  no wallet-to-bank withdrawals). (§8)
- **Accounts** via OAuth (Google / Apple / GitHub / Meta) **plus a password**
  (required to authorize sensitive actions and to remove the account/effects from
  a machine).
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
  ├─ activate.vance   — payment webhooks → entitlements → signed entitlement tokens + license leases
  ├─ commerce.vance   — seller processor onboarding (direct-charge), payment links, 20% application fee, sales ledger
  └─ build.vance      — hosted build/sign service for the creator tiers (later phase)

Third parties  [gated]
  ├─ Seller's connected processor (Stripe/PayPal) — buyer pays the SELLER directly;
  │    Vance takes a 20% application fee + gets a "paid" webhook to auto-activate (§8a.1)
  └─ (licensing/activation/leases/revocation is built in-house on this backend;
     Keygen CE self-host is the free fallback — see §16.2)

  (No merchant-of-record and no payouts: money lands in the SELLER's account, never
   pools in Vance. Vance only ever receives its 20% fee and never pays anyone out.)
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
- **Device caps (chosen):** **free effects = unlimited** machines; **paid effects
  = a hard cap of 2 active machines** per account. Activating a paid effect on a
  **3rd machine auto-drops the oldest** active device (no "free a slot first"
  step); the dropped device's lease stops renewing and it uninstalls the effect on
  its next heartbeat/launch. Enforced server-side at activation/heartbeat: only the
  2 most-recent active `device_installs` for a paid sku renew. Still track all
  devices + show Manage-Devices; layer abuse signals (many new devices/hr,
  impossible-travel) on top.

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

## 6. Entitlements — auto-granted on payment, "up to 2 machines"

- Entitlements are **user-scoped**: an owned effect may run on the account's
  active devices — **unlimited for free effects, up to 2 for paid** (§5).
- **The processor `payment.succeeded` webhook is the entitlement trigger** (§8):
  on a confirmed paid charge, the server inserts the `entitlement` row and grants
  it **directly to the buyer's account** (recorded as an `activation` linked to
  the `sale`). Nothing for the buyer to redeem or paste. Free effects create the
  entitlement on first install with no payment.
- Server issues a **signed entitlement token** (Ed25519 JWT: `{sub, did,
  skus:[…], iat, exp=iat+3d, ent_ver}`) on login / heartbeat /
  `entitlement.changed`; cached in the keychain. `exp` is the **offline grace
  window** (buyer works ~3 days with no internet; while online it silently renews).
- **Enforce at load:** before instantiating a paid effect, `host.rs` checks a
  currently-valid entitlement token lists that sku **and** this device is one of
  the ≤2 active for it. Free effects skip both checks.
- **Install / auto-install:** reconcile = "for each active entitlement not on
  this device (and within the 2-device cap), install it." Manual (click Install)
  or **Auto** (Settings toggle `autoInstallPurchases`, per device) → a background
  task installs newly-entitled skus silently. Install reuses the manifest
  pipeline: `GET /v1/artifacts/{sku}?platform=…` returns `{url, sha256, dest}`
  where `url` is a **short-lived signed URL** minted only after the server
  re-checks entitlement + device slot; client verifies the sha256 pin (unchanged)
  and places the file.
- **Revocation** flows through the **same** `entitlement.changed` path →
  `state=revoked` → uninstall. Triggers: the processor's **`refund` / `dispute`
  webhook** (buyer got their money back → effect is pulled), or a seller/admin
  action. Vance itself never charged the buyer, so there's no Vance-side refund to
  issue — the money movement lives entirely on the seller's processor.

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
**Licensing layer — decided (see §16): build it into our own backend.** Our needs
are light (per-user entitlement, 2-device cap + auto-drop-oldest, 3-day
lease/heartbeat, revocation, Ed25519-signed offline license) — a small addition to
the accounts/entitlements backend already built in M1–M2, using standard primitives
(`ed25519-dalek` + libsodium sealed box), no second runtime to operate. **Keygen
CE** (self-hostable, **free for commercial use** under the Fair Core License, and it
ships exactly these primitives) is the documented **zero-lock-in fallback** if we'd
rather offload the security-critical license logic than own it. Either way we write
the **decrypt-on-load shim**.

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
   skus, fingerprint, 3-day expiry, buyer id for watermark, and the **signed
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

## 8. Monetization — connected-processor auto-activation (Vance never holds funds)  [chosen model]

> Replaces the earlier Merchant-of-Record + Stripe-Connect *payout* design (kept
> in git history). Buyer money lands in the **seller's own** account; Vance only
> ever receives its 20% fee and never pays anyone out.

**Core principle: Vance never holds buyer funds and never pays anyone out — it only
ever receives its 20% fee.** Buyer money flows straight into the *seller's*
connected Stripe/PayPal account; Vance takes a **20% application fee** at charge
time and gets a webhook to activate. No merchant-of-record pooling of consumer
funds, no seller wallet/withdrawals, no marketplace-facilitator VAT/sales-tax
stack across 100+ jurisdictions.

- **Free effects:** unlimited machines, install exactly like today. No money, no cap.
- **Paid effects:** capped at **2 active machines** per account (deactivate one to
  add another).

**Chosen parameters:** **20%** platform fee (seller keeps ~80%), taken **per sale**
as an application fee; activation is a **direct grant** to the buyer's account (no
code to paste); collected via **connected-processor auto-activation** — the single
paid path (no manual off-platform pay). This is what makes the model both
frictionless *and* scam-resistant (§8a).

### The flow (all automatic)
1. **Seller onboards once:** links their own Stripe/PayPal to Vance (direct-charge
   mode). Required to list a *paid* effect. The seller is the **merchant of
   record**; the money is theirs.
2. Marketplace lists the effect with its **price**. Buyer taps **Get**.
3. Buyer pays on a **Vance-generated payment link on the seller's connected
   account** (card / Apple Pay / Google Pay / PayPal, in the system browser). The
   **funds land directly in the seller's account** — never in Vance; Vance's **20%
   is skimmed as an application fee** at charge time.
4. The processor's **`payment.succeeded` webhook → Vance** → Vance **auto-grants
   the entitlement** to the buyer's account (an `activation` row; nothing to paste
   — the effect just appears + installs; 2-machine cap; the paid `.wasm` decrypts +
   runs via the §7 gate). Seconds, not a manual step.
5. **Take-and-run is impossible:** delivery is triggered by the *processor's* proof
   of payment, not by the seller choosing to act — the seller cannot collect and
   withhold (§8a).

### Why this stays operationally simple
- **Vance never holds or moves buyer money** → not a fund-pooling facilitator; no
  payouts, no seller wallet-to-bank withdrawals.
- **Seller is merchant of record** (direct charges) → the **seller** owns the sales
  tax on their sale (Stripe Tax can run on their connected account); Vance owes tax
  only on **its 20% fee** — its own revenue.
- **Buyer keeps processor-backed chargeback/dispute rights** — genuine protection
  Vance doesn't have to fund.

> **[honest caveats]**
> - **Sellers must use a supported processor** (Stripe/PayPal). A creator who won't
>   connect one can't sell *paid* effects here (they can still publish free ones).
>   That's the deliberate price of eliminating take-and-run.
> - **"Marketplace facilitator" tax laws can still reach the platform** in some US
>   states / the EU regardless of the direct-charge structure — a **legal-review
>   item**, not a blocker. Direct charges make the *seller* MoR, the simplest
>   defensible posture, but confirm with counsel before GA.
> - **Processor fees** (~2.9% + 30¢) come out of the seller's 80% — standard for
>   any online sale.
> - **Refunds/chargebacks** are the seller's + processor's to handle; a refunded
>   payment flows through `entitlement.changed` → revoke (§6), so a refunded buyer
>   loses the effect.

### 8a. Anti-scam & buyer safety

Take-and-run is already **structurally impossible** under §8 (delivery is fired by
the processor's proof of payment, not the seller's goodwill). These layers harden
the rest — bad/fake effects, impersonation, review spam — and keep the marketplace
trustworthy:

**1. (Foundation) Auto-activation on verified payment — see §8.** The buyer's money
and the effect's delivery are bound to the *same* processor event, so a seller
can't be paid without the buyer being served, and the buyer keeps processor
chargeback rights. Everything below assumes this base.

**2. Verified identity for paid sellers (required).** Real name + verified email +
phone, and a verified payout/processor account. A scammer who must show ID mostly
doesn't try, and is traceable/reportable if they do. This is the biggest single
deterrent.

**3. Progressive trust — cap the blast radius of a new seller.** New/unverified
sellers face **price caps**, a **"new seller" badge**, a **daily activation cap**,
and optional **manual review of first N sales**. Trust (higher caps, faster
listing) is *earned* by a clean track record. A day-one scammer can't list a $200
effect.

**4. Reputation, in the open.** Purchase-gated reviews/ratings, a **visible
dispute/complaint rate**, and a **successful-activations count** on every seller +
listing. Buyers see risk before paying.

**5. Report → auto-freeze → delist/ban.** One-tap "this effect is a scam / doesn't
work." A seller whose complaint rate crosses a threshold is **auto-frozen**
(listings hidden, no new activations) pending review; confirmed scammers are banned
and their verified identity blocked from re-registering.

**6. (Optional) Seller trust bond.** A seller may post a refundable bond to earn a
**"buyer-protected" badge** that covers edge cases the processor won't (e.g. "the
effect doesn't do what was advertised" where a chargeback is contested); paid **from
the bond**, i.e. the *seller's* money. Opt-in for premium sellers; don't require it.

> **The honest bottom line:** because payment and delivery are bound to the same
> processor event (§8), **take-and-run is structurally impossible** and money never
> touches Vance. The residual risks are ordinary marketplace ones — a *bad or
> mis-described* effect, impersonation, review spam — handled by verified identity,
> progressive-trust caps, open reputation, and report→freeze→ban, plus the buyer's
> processor chargeback rights. No escrow needed, and nothing here requires Vance to
> hold or send buyer funds.

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
domain-verified** brands get a badge; **paid sellers must be verified** — which
**Stripe Connect onboarding does for us**: for Standard accounts Stripe performs
the seller's identity verification + KYC/AML, so Vance gates paid-seller status on
the connected account reaching `charges_enabled=true` rather than building its own
KYC (§16). A core buyer-trust safeguard alongside auto-activation (§8, §8a).

**Reviews:** 1–5 stars + text, **gated to users who installed/purchased** (kills
most spam), aggregate on the card, velocity limits + moderation queue.

**In-app flow (paid):** Browse → item page (screenshots, reviews, price, seller,
requested capabilities) → **Get** opens the seller's connected-processor checkout
in the system browser (card / Apple Pay / Google Pay / PayPal) → buyer pays (funds
→ seller's account; Vance's 20% skimmed as an application fee) → the processor's
**`payment.succeeded` webhook auto-grants** the entitlement to the buyer's account
(§8) → Install auto-starts (download, verify sha256 + Ed25519 against the pinned
manifest, **diff requested capabilities vs sandbox policy and prompt for any
elevated capability**) → run sandboxed. Free effects skip straight from Browse to
Install. Updates = version bump + re-verify; **flag ownership transfers before
auto-updating** (supply-chain guard).

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
- **My Effects** — owned effects (free + purchased), per-device Install/Uninstall
  state, `autoInstallPurchases` toggle. Paid effects show their 2 device slots.
- **Selling** (sellers) — **connect Stripe/PayPal** (required to sell paid),
  list/manage effects + prices, a **sales ledger** (each sale, the buyer's
  payment, Vance's 20% fee) sourced from the processor, and verified-seller status.
  (No wallet/withdraw screen — the money is already in the seller's own account.)
- Existing **privacy** opt-ins move here too.

---

## 12. Data model (Postgres-shaped)

```
users(id, email citext unique, email_verified, password_hash null, password_set_at, created_at, updated_at)
oauth_identities(id, user_id, provider{google,apple,github,meta}, provider_subject, email_at_provider, linked_at, UNIQUE(provider, provider_subject))
sessions(id=session_id, user_id, device_id, device_name, platform, app_version, refresh_token_hash, refresh_generation, created_at, last_seen_at, ip_last, revoked_at null, revoked_reason null, UNIQUE(user_id, device_id))
entitlements(id, user_id, sku, source{sale,grant,free}, state{active,expired,revoked}, granted_at, expires_at null, activation_id null, UNIQUE(user_id, sku))
seller_processor_accounts(seller_id, processor{stripe,paypal}, account_ref, charges_enabled, status, connected_at)   -- direct-charge; Vance never holds funds
sales(id, sku, seller_id, buyer_user_id, price, currency, processor, processor_charge_ref, application_fee, status{paid,refunded,disputed}, created_at)   -- money lands in SELLER's account; Vance skims application_fee
activations(id, sale_id, sku, user_id, state{active,revoked}, granted_at, revoked_at null)   -- created by payment.succeeded webhook
plugin_artifacts(sku, version, platform, object_key, sha256, dest)   -- entitlement-gated serve
device_installs(session_id, sku, installed_version, installed_at, uninstalled_at null)   -- ≤2 active rows per paid sku per user
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
4. **M4 — Paid effects (connected-processor auto-activation).** Seller connects
   Stripe/PayPal (direct-charge + application fee), listing with price, buyer
   checkout in the system browser, **`payment.succeeded` webhook → auto-grant** to
   the buyer's account, **direct-grant entitlement**, paid-effect **encryption +
   decrypt-on-load shim** + in-house license endpoints (§16.2), 2-device cap with
   auto-drop-oldest, **grant on `payment.succeeded` / revoke on
   `charge.refunded` + `charge.dispute.created`** (Connect webhooks, signature +
   idempotency verified).
5. **M5 — Anti-scam + seller trust (§8a).** Verified-seller identity (required for
   paid), progressive-trust caps for new sellers, "Selling" panel (connected
   processor + sales ledger), owner-gated reviews/ratings + visible dispute rate,
   one-tap report → auto-freeze → delist/ban, optional seller trust bond. *(This is
   what keeps the marketplace trustworthy, so it's not optional.)*
6. **M6 — Creator tooling.** Phase 0 starter template → Phase 1 Effect Studio
   (ISF shaders) → Phase 2 node graph → Phase 3 hosted build.

M1–M2 are **client + backend fundamentals** and unblock everything. M3 delivers
visible value (browse/install free) without any money. Paid (M4+) needs only the
**seller's connected processor** (direct-charge + application fee) — no MoR, no
payouts, no fund-pooling; Vance only ever receives its 20% fee.

---

## 14. Decisions

**Decided:**
- **Payment model:** **connected-processor auto-activation** — the single paid
  path. Seller links their own Stripe/PayPal (direct-charge); buyer pays on it;
  the `payment.succeeded` webhook auto-grants the effect. No manual off-platform
  pay. Money lands in the seller's account; Vance never holds funds.
- **Platform fee = 20%**, taken **per sale** as an application fee.
- **Direct-grant activation** — effect lands on the buyer's account automatically
  (no code to paste).
- **Device cap:** free = unlimited, paid = 2 active machines.

- **Processor: Stripe Connect** (direct charges + application fee). Direct charges
  make the connected seller the merchant of record and keep Vance out of the fund
  flow; Apple/Google Pay come free via Stripe's hosted checkout in the system
  browser. (PayPal can be added later to broaden reach.)
- **3rd-machine policy: auto-drop the oldest** active device when a paid effect is
  activated on a 3rd — no "free a slot first" friction. The dropped device
  uninstalls the effect on its next heartbeat/launch (§5).
- **Offline grace window: 3 days.** Short = fast revocation + tighter sharing
  control, at the cost of offline tolerance.
  > **[note — flag for video editors]** 3 days is aggressive for this audience:
  > a pro editing offline on a multi-day shoot/flight/remote location would see
  > paid effects disable until they reconnect. Free effects are unaffected. If
  > offline complaints appear, 7 days is the usual middle ground. Keeping 3 as
  > chosen.

**Still open:**
- **Distribution:** self-installer + Microsoft Store carry the marketplace; **Mac
  App Store build ships without in-app buying** — confirm.

*(Licensing build-vs-Keygen and the minimal-legal-exposure posture were researched
and resolved — see **§16**.)*

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
6. **"Take-and-run" is designed out, but bad/mis-described effects aren't.**
   Because payment and delivery share one processor event (§8), a seller can't be
   paid without the buyer being served, and the buyer keeps processor chargeback
   rights — no escrow needed, money never touches Vance. The residual risks are
   ordinary marketplace ones (an effect that underdelivers, impersonation, review
   spam), handled by verified identity, progressive-trust caps, open reputation,
   and report→freeze→ban (§8a). Those safeguards are **required**, not optional.
7. **Sellers must connect a supported processor** (Stripe/PayPal) to sell paid
   effects — a creator who won't can only publish free effects.
8. **US tax is simpler, not immune.** Direct charges make the *seller* merchant of
   record (they own their sale's tax; Stripe Tax can run on their account), and
   Vance owes tax only on its **20% fee**. But **"marketplace-facilitator"** is a
   *statutory* status decided per US state independent of who's MoR — a state can
   still deem Vance a facilitator (the "indirectly…collects the payment" prong is
   broad, and Vance runs the whole checkout + auto-grant). Seller-ToS tax
   allocation binds the seller, **not a tax authority** (it only gives Vance an
   indemnity claim). Real early exposure is **low**: the duty only bites once an
   effect is taxable in a state *and* aggregate **facilitated** sales cross that
   state's economic-nexus threshold (~$100k/200 txns, measured on sellers'
   *combined* volume). Monitor per-state; get a state-tax opinion before nearing
   $100k anywhere. (§16)
9. **EU VAT is the real gate — likely a launch blocker for EU consumer sales.**
   Under **Art. 9a** of the VAT Implementing Regulation (upheld by the CJEU in
   *Fenix/OnlyFans*, C-695/20, 2023 — a platform that, exactly like us, took a ~20%
   cut), a platform "facilitating" electronically-supplied services is presumed the
   **deemed supplier** liable to collect + remit VAT on B2C EU sales. The
   presumption is **non-rebuttable** if the platform authorises delivery or sets the
   general terms — and our **auto-grant-on-webhook** authorises delivery and we set
   platform T&Cs. **Stripe direct charges / seller-as-MoR do NOT help here** (Art. 9a
   turns on control of terms/delivery, not who holds funds), and there's **no
   small-seller cross-border threshold** to hide behind. So EU B2C either needs a
   real VAT advisor + (non-Union) OSS registration, or a flow restructured so the
   *seller* sets terms + authorises delivery. **Recommendation: launch US-first;
   gate EU consumer sales on that advice.** (§16)
10. **"Money transmitter" is a legal question, not a config flag.** Funds never
    touching Vance's balance *lowers* the risk but is **not** a safe harbor under US
    state MTL / FinCEN rules — a payments/fintech lawyer must confirm before launch.
    (§16)

---

## 16. Decisions resolved (researched 2026-08)

Grounded in a research pass (Stripe Connect docs, marketplace-facilitator statutes,
EU VAT Art. 9a + CJEU *Fenix*, DSA, Keygen licensing) with an adversarial
fact-check. **This is engineering orientation, not legal advice** — the two items
tagged **[lawyer]** genuinely need a qualified advisor before GA.

### 16.1 Stripe Connect — exact configuration
- **Standard accounts + Direct charges + `application_fee_amount` = 20%**, sold via
  **Stripe-hosted Checkout opened in the system browser**. This one combo:
  seller = merchant of record; **buyer funds never touch Vance's balance** (only the
  20% fee routes to Vance); refund / chargeback / negative-balance / fraud liability
  sits with the **seller + Stripe, not Vance**; **Apple Pay / Google Pay free** on
  hosted Checkout; **Stripe owns KYC/AML** onboarding (satisfies our verified-seller
  requirement — gate on `charges_enabled=true`).
- **Correction to earlier draft:** use plain **Standard** accounts. Standard/Express/
  Custom are **not** deprecated (the Accounts-v2 "controller properties" migration is
  *optional* + backwards-compatible) — don't over-engineer onto v2.
- **Don't use** destination charges or separate charges-and-transfers — both make
  **Vance** the MoR and pull buyer funds through Vance's balance.
- **Build gotchas (bake into M4):**
  - **Grant on `payment.succeeded`, revoke on `charge.refunded` / `charge.dispute.created`.**
    Auto-grant + a later chargeback debits the *seller*, but the buyer keeps the
    delivered effect unless we revoke — wire both webhooks → `entitlement.changed`.
  - **Listen on the *connected account's* Connect webhooks** (they carry the account
    id), **verify signatures + idempotency**.
  - **Application fee is NOT auto-refunded** — on a seller refund, pass
    `refund_application_fee` per our policy (decide: do we return our 20%?).
  - **Radar blind spot:** platform Radar rules don't apply to direct charges on the
    seller's account — fraud screening is largely Stripe-default.

### 16.2 Licensing layer — build it (Keygen CE is the free fallback)
- **"Which is free":** both. **Keygen CE** self-hosts **free for commercial use**
  (Fair Core License; its narrow non-compete only bites if you resell *Keygen-like
  licensing* — irrelevant to us; converts to Apache-2.0 two years post-release) and
  ships every primitive we need (Ed25519 offline license files, machine activation +
  limits, heartbeats/leases, revocation). Building it ourselves has no license fee
  either.
- **Decision: build it into our own backend.** Reasons: needs are light (~3 endpoints
  — activate / heartbeat / revoke — plus Ed25519 signing), we already designed the
  scheme (§7.2) and are already standing up the accounts/entitlements backend, the
  per-version CEK-sealing is bespoke on top of Keygen anyway, and it avoids operating
  a second runtime (Ruby + Postgres + Redis/Sidekiq). **Keygen CE stays the
  documented, zero-lock-in fallback** if we later prefer to offload the
  security-critical license logic.

### 16.3 Legal posture — minimal-issue, honestly bounded
- **US (minimal-issue, low early exposure):** direct charges (seller = MoR) + a
  **Seller Agreement** that allocates all sales-tax responsibility to the seller +
  **per-state facilitated-volume monitoring**. Real collect-and-remit duty only
  triggers once an effect is taxable in a state *and* aggregate facilitated sales
  cross its economic-nexus threshold — so early exposure is low, but it scales
  silently with marketplace volume. **[lawyer]** state-tax opinion before nearing
  $100k in any state.
- **EU (the one hard gate):** **[lawyer]** the Art. 9a **deemed-supplier VAT** risk
  is real and *not* fixed by the payment architecture (§15.9). **Recommended posture:
  launch US-first (+ other non-EU markets); gate EU consumer sales** on a VAT opinion
  and, if needed, (non-Union) **OSS** registration — or restructure so the seller
  sets terms + authorises delivery.
- **Money transmitter:** **[lawyer]** low-risk posture (Vance never holds/forwards
  buyer funds) but confirm under state MTL / FinCEN.
- **DSA trader verification (Art. 30 KYBC):** **does not apply at our size** (Art. 29
  exempts <50 staff / ≤€10M). Still do the cheap baseline (clear T&Cs, notice-and-
  action, a point of contact) and **capture Stripe-KYC seller data now** so the
  Art. 30 obligation is a non-event if we grow past the threshold.
- **Minimal legal doc set to draft (counsel-reviewed once):** Seller Agreement
  (80/20 split, IP/originality warranty + indemnity, seller = seller-of-record
  responsible for all tax), Buyer Marketplace ToS, a default **EULA** (applies when a
  seller supplies none), Privacy Policy (GDPR + CCPA), refund/dispute policy
  (seller-owned), DMCA + DSA notice-and-action + acceptable-use. Wire **Stripe
  Connected Account Agreement** acceptance into onboarding.

### 16.4 Net
The architecture (Stripe **Standard + direct charges + 20% app fee**, licensing
**built in-house**) is the genuinely minimal-cost, minimal-US-exposure setup and
needs **no external vendor beyond Stripe**. The only place "minimal" still demands a
real advisor is **EU VAT** — so **ship US-first** and treat EU B2C as a deliberate,
advised second step.
