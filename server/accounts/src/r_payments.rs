//! M4b — Stripe Connect payments (spec §8, §8a.1). Vance never holds buyer
//! funds: the buyer pays on the SELLER's connected account (direct charge), Vance
//! takes a 20% application fee, and the `payment.succeeded` webhook auto-grants
//! the effect. Refund/dispute webhooks revoke it.
//!
//! The checkout + connect endpoints call the real Stripe API and are the go-live
//! gate (they need `VANCE_STRIPE_SECRET_KEY`); until configured they return a
//! clear "not configured". The webhook — the auto-activation brain — is fully
//! testable with a signed synthetic event.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::{AppState, ServerEvent};
use crate::util::{new_id, now};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

const PLATFORM_FEE_BPS: i64 = 2000; // 20%

// ===========================================================================
// Webhook  (POST /webhooks/stripe) — no auth; the Stripe signature is the proof.
// ===========================================================================

pub async fn webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> AppResult<Json<Value>> {
    let secret = match &st.config.stripe_webhook_secret {
        Some(s) => s.clone(),
        // Not configured → acknowledge so Stripe doesn't retry forever.
        None => return Ok(Json(json!({ "ignored": "stripe not configured" }))),
    };
    let sig = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_stripe_signature(&secret, &body, sig) {
        return Err(AppError::BadRequest("invalid stripe signature".into()));
    }

    let event: Value = serde_json::from_str(&body).map_err(|_| AppError::BadRequest("bad json".into()))?;
    let event_id = event["id"].as_str().unwrap_or("").to_string();
    let kind = event["type"].as_str().unwrap_or("").to_string();
    if event_id.is_empty() {
        return Err(AppError::BadRequest("missing event id".into()));
    }

    // Idempotency: first insert wins; a re-delivery is a no-op.
    let inserted = sqlx::query("INSERT OR IGNORE INTO stripe_events (event_id, kind, received_at) VALUES (?,?,?)")
        .bind(&event_id)
        .bind(&kind)
        .bind(now())
        .execute(&st.pool)
        .await?;
    if inserted.rows_affected() == 0 {
        return Ok(Json(json!({ "duplicate": true })));
    }

    let obj = &event["data"]["object"];
    match kind.as_str() {
        "checkout.session.completed" | "payment_intent.succeeded" => {
            if let (Some(user_id), Some(sku)) = metadata_ids(obj) {
                grant(&st, &user_id, &sku, "sale").await?;
            }
        }
        "charge.refunded" | "charge.dispute.created" | "charge.disputed" => {
            if let (Some(user_id), Some(sku)) = metadata_ids(obj) {
                revoke(&st, &user_id, &sku).await?;
            }
        }
        "account.updated" => {
            if let Some(account_id) = obj["id"].as_str() {
                let enabled = obj["charges_enabled"].as_bool().unwrap_or(false);
                sqlx::query(
                    "UPDATE stripe_connected_accounts SET charges_enabled=?, updated_at=? WHERE account_id=?",
                )
                .bind(enabled as i64)
                .bind(now())
                .bind(account_id)
                .execute(&st.pool)
                .await?;
            }
        }
        _ => {}
    }
    Ok(Json(json!({ "received": true })))
}

/// `vance_user_id` + `vance_sku` are stamped into the checkout session +
/// payment-intent metadata, so both the paid and refund/dispute events carry them.
fn metadata_ids(obj: &Value) -> (Option<String>, Option<String>) {
    let md = &obj["metadata"];
    (
        md["vance_user_id"].as_str().map(String::from),
        md["vance_sku"].as_str().map(String::from),
    )
}

fn verify_stripe_signature(secret: &str, payload: &str, sig_header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut ts = None;
    let mut v1s = Vec::new();
    for part in sig_header.split(',') {
        if let Some(v) = part.trim().strip_prefix("t=") {
            ts = Some(v);
        } else if let Some(v) = part.trim().strip_prefix("v1=") {
            v1s.push(v);
        }
    }
    let Some(ts) = ts else { return false };
    let signed = format!("{ts}.{payload}");
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(signed.as_bytes());
    let expected = mac.finalize().into_bytes();
    v1s.iter()
        .any(|v| hex_decode(v).map(|b| ct_eq(&b, &expected)).unwrap_or(false))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    d == 0
}

// ===========================================================================
// grant / revoke (shared with the entitlement flow)
// ===========================================================================

async fn grant(st: &AppState, user_id: &str, sku: &str, source: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO entitlements (id, user_id, sku, source, state, granted_at)
         VALUES (?,?,?,?,'active',?)
         ON CONFLICT(user_id, sku) DO UPDATE SET state='active', source=excluded.source, granted_at=excluded.granted_at",
    )
    .bind(new_id())
    .bind(user_id)
    .bind(sku)
    .bind(source)
    .bind(now())
    .execute(&st.pool)
    .await?;
    bump_and_emit(st, user_id, sku, "active").await
}

async fn revoke(st: &AppState, user_id: &str, sku: &str) -> AppResult<()> {
    sqlx::query("UPDATE entitlements SET state='revoked' WHERE user_id=? AND sku=?")
        .bind(user_id)
        .bind(sku)
        .execute(&st.pool)
        .await?;
    sqlx::query("UPDATE device_installs SET uninstalled_at=? WHERE user_id=? AND sku=? AND uninstalled_at IS NULL")
        .bind(now())
        .bind(user_id)
        .bind(sku)
        .execute(&st.pool)
        .await?;
    bump_and_emit(st, user_id, sku, "revoked").await
}

async fn bump_and_emit(st: &AppState, user_id: &str, sku: &str, state: &str) -> AppResult<()> {
    sqlx::query("UPDATE users SET ent_ver=ent_ver+1, updated_at=? WHERE id=?")
        .bind(now())
        .bind(user_id)
        .execute(&st.pool)
        .await?;
    let ev: i64 = sqlx::query("SELECT ent_ver FROM users WHERE id=?")
        .bind(user_id)
        .fetch_optional(&st.pool)
        .await?
        .map(|r| r.get::<i64, _>("ent_ver"))
        .unwrap_or(0);
    st.emit(ServerEvent {
        user_id: user_id.to_string(),
        session_id: None,
        kind: "entitlement.changed".into(),
        data: json!({ "sku": sku, "state": state, "ent_ver": ev }),
    });
    Ok(())
}

// ===========================================================================
// Buyer checkout  (POST /v1/checkout) — real Stripe call, gated on config.
// ===========================================================================

#[derive(Deserialize)]
pub struct CheckoutParams {
    sku: String,
    success_url: Option<String>,
    cancel_url: Option<String>,
}

pub async fn checkout(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<CheckoutParams>,
) -> AppResult<Json<Value>> {
    let secret = st
        .config
        .stripe_secret
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("payments aren't set up on this server yet".into()))?;
    let row = sqlx::query(
        "SELECT display_name, price_cents, stripe_account FROM catalog_effects WHERE sku=? AND listed=1",
    )
    .bind(&p.sku)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::NotFound("no such effect".into()))?;
    let price: i64 = row.get("price_cents");
    if price <= 0 {
        return Err(AppError::BadRequest("this effect is free".into()));
    }
    let account: String = row
        .get::<Option<String>, _>("stripe_account")
        .ok_or_else(|| AppError::BadRequest("the seller hasn't connected payouts yet".into()))?;
    let name: String = row.get("display_name");

    let fee = (price * PLATFORM_FEE_BPS / 10_000).to_string();
    let price_s = price.to_string();
    let success = p
        .success_url
        .unwrap_or_else(|| format!("{}/pay/success", st.config.public_url));
    let cancel = p
        .cancel_url
        .unwrap_or_else(|| format!("{}/pay/cancel", st.config.public_url));

    // Direct charge on the connected account + a 20% application fee. Buyer money
    // lands in the SELLER's balance; Vance never holds it (spec §8).
    let client = reqwest::Client::new();
    let v: Value = client
        .post("https://api.stripe.com/v1/checkout/sessions")
        .bearer_auth(secret)
        .header("Stripe-Account", &account)
        .form(&[
            ("mode", "payment"),
            ("success_url", success.as_str()),
            ("cancel_url", cancel.as_str()),
            ("line_items[0][quantity]", "1"),
            ("line_items[0][price_data][currency]", "usd"),
            ("line_items[0][price_data][unit_amount]", price_s.as_str()),
            ("line_items[0][price_data][product_data][name]", name.as_str()),
            ("payment_intent_data[application_fee_amount]", fee.as_str()),
            ("metadata[vance_user_id]", user.claims.sub.as_str()),
            ("metadata[vance_sku]", p.sku.as_str()),
            ("payment_intent_data[metadata][vance_user_id]", user.claims.sub.as_str()),
            ("payment_intent_data[metadata][vance_sku]", p.sku.as_str()),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("stripe: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("stripe json: {e}")))?;
    let url = v["url"].as_str().ok_or_else(|| {
        AppError::Internal(format!(
            "stripe checkout failed: {}",
            v.get("error").map(|e| e.to_string()).unwrap_or_default()
        ))
    })?;
    Ok(Json(json!({ "checkout_url": url })))
}

// ===========================================================================
// Seller Connect onboarding  (real Stripe calls, gated on config).
// ===========================================================================

pub async fn connect_onboard(
    State(st): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Value>> {
    let secret = st
        .config
        .stripe_secret
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("payments aren't set up on this server yet".into()))?;
    let client = reqwest::Client::new();

    let existing = sqlx::query("SELECT account_id FROM stripe_connected_accounts WHERE user_id=?")
        .bind(&user.claims.sub)
        .fetch_optional(&st.pool)
        .await?;
    let account_id = if let Some(r) = existing {
        r.get::<String, _>("account_id")
    } else {
        let v: Value = client
            .post("https://api.stripe.com/v1/accounts")
            .bearer_auth(secret)
            .form(&[("type", "standard")])
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("stripe: {e}")))?
            .json()
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let id = v["id"]
            .as_str()
            .ok_or_else(|| AppError::Internal("stripe account create failed".into()))?
            .to_string();
        sqlx::query(
            "INSERT INTO stripe_connected_accounts (user_id, account_id, created_at, updated_at) VALUES (?,?,?,?)",
        )
        .bind(&user.claims.sub)
        .bind(&id)
        .bind(now())
        .bind(now())
        .execute(&st.pool)
        .await?;
        id
    };

    let refresh = format!("{}/connect/refresh", st.config.public_url);
    let ret = format!("{}/connect/return", st.config.public_url);
    let link: Value = client
        .post("https://api.stripe.com/v1/account_links")
        .bearer_auth(secret)
        .form(&[
            ("account", account_id.as_str()),
            ("refresh_url", refresh.as_str()),
            ("return_url", ret.as_str()),
            ("type", "account_onboarding"),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("stripe: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let url = link["url"]
        .as_str()
        .ok_or_else(|| AppError::Internal("stripe account_link failed".into()))?;
    Ok(Json(json!({ "onboard_url": url })))
}

pub async fn connect_status(
    State(st): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT charges_enabled FROM stripe_connected_accounts WHERE user_id=?")
        .bind(&user.claims.sub)
        .fetch_optional(&st.pool)
        .await?;
    Ok(Json(match row {
        Some(r) => json!({ "connected": true, "charges_enabled": r.get::<i64, _>("charges_enabled") != 0 }),
        None => json!({ "connected": false, "charges_enabled": false }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_accepts_valid_and_rejects_tampered() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret = "whsec_test";
        let payload = r#"{"id":"evt_1","type":"payment_intent.succeeded"}"#;
        let ts = "1700000000";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{ts}.{payload}").as_bytes());
        let sig = mac.finalize().into_bytes();
        let hexsig: String = sig.iter().map(|b| format!("{b:02x}")).collect();

        let header = format!("t={ts},v1={hexsig}");
        assert!(verify_stripe_signature(secret, payload, &header));
        assert!(!verify_stripe_signature("wrong", payload, &header));
        assert!(!verify_stripe_signature(secret, "tampered", &header));
        assert!(!verify_stripe_signature(secret, payload, "t=1,v1=deadbeef"));
    }
}
