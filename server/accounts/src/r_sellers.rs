//! M5 — seller trust + anti-scam (spec §8a). Reports drive a visible dispute
//! rate + auto-freeze; sellers earn trust (which lifts new-seller caps); browse
//! hides frozen/banned sellers and requires a verified seller for PAID listings.

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::util::{new_id, now};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

/// New (untrusted) sellers can't list above this until they've earned trust.
pub const NEW_SELLER_PRICE_CAP_CENTS: i64 = 2000;
const AUTO_FREEZE_MIN_REPORTS: i64 = 3;
const AUTO_FREEZE_RATE: f64 = 0.10;

fn dev_only(st: &AppState) -> AppResult<()> {
    if st.config.dev_provider {
        Ok(())
    } else {
        Err(AppError::Forbidden("dev endpoints are disabled".into()))
    }
}

// ---------------------------------------------------------------------------
// POST /v1/catalog/:sku/report — report a bad/mis-described/impersonating effect.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReportParams {
    reason: Option<String>,
}

pub async fn report(
    State(st): State<AppState>,
    user: AuthUser,
    Path(sku): Path<String>,
    Json(p): Json<ReportParams>,
) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT seller_id, downloads FROM catalog_effects WHERE sku=?")
        .bind(&sku)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("no such effect".into()))?;
    let seller_id: Option<String> = row.get("seller_id");

    // One report per (user, effect).
    sqlx::query(
        "INSERT OR IGNORE INTO reports (id, sku, reporter_user_id, reason, created_at) VALUES (?,?,?,?,?)",
    )
    .bind(new_id())
    .bind(&sku)
    .bind(&user.claims.sub)
    .bind(&p.reason)
    .bind(now())
    .execute(&st.pool)
    .await?;

    // Auto-freeze the seller if this effect's complaint rate crosses the bar.
    if let Some(seller) = seller_id {
        let reports: i64 = sqlx::query("SELECT COUNT(*) AS n FROM reports WHERE sku=?")
            .bind(&sku)
            .fetch_one(&st.pool)
            .await?
            .get("n");
        let downloads: i64 = row.get("downloads");
        let rate = if downloads > 0 {
            reports as f64 / downloads as f64
        } else if reports >= AUTO_FREEZE_MIN_REPORTS {
            1.0
        } else {
            0.0
        };
        if reports >= AUTO_FREEZE_MIN_REPORTS && rate >= AUTO_FREEZE_RATE {
            upsert_seller(&st, &seller, None, None, None, Some(true), None).await?;
        }
    }
    Ok(Json(json!({ "reported": true })))
}

// ---------------------------------------------------------------------------
// GET /v1/seller — the caller's seller status (badges).
// ---------------------------------------------------------------------------

pub async fn status(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    Ok(Json(seller_badges(&st, &user.claims.sub).await?))
}

/// The caller's listings with downloads + report counts (the Selling ledger).
pub async fn my_effects(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT sku, display_name, price_cents, downloads,
                (SELECT COUNT(*) FROM reports r WHERE r.sku=c.sku) AS reports
         FROM catalog_effects c WHERE seller_id=? ORDER BY created_at DESC",
    )
    .bind(&user.claims.sub)
    .fetch_all(&st.pool)
    .await?;
    let effects: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "sku": r.get::<String,_>("sku"),
                "display_name": r.get::<String,_>("display_name"),
                "price_cents": r.get::<i64,_>("price_cents"),
                "downloads": r.get::<i64,_>("downloads"),
                "reports": r.get::<i64,_>("reports"),
            })
        })
        .collect();
    Ok(Json(json!({ "effects": effects })))
}

// ---------------------------------------------------------------------------
// Dev-only: set the caller's seller flags (stand-in for KYC + moderation).
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DevSellerParams {
    verified: Option<bool>,
    trusted: Option<bool>,
    frozen: Option<bool>,
    banned: Option<bool>,
    display_name: Option<String>,
}

pub async fn dev_set(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<DevSellerParams>,
) -> AppResult<Json<Value>> {
    dev_only(&st)?;
    upsert_seller(
        &st,
        &user.claims.sub,
        p.display_name.as_deref(),
        p.verified,
        p.trusted,
        p.frozen,
        p.banned,
    )
    .await?;
    Ok(Json(seller_badges(&st, &user.claims.sub).await?))
}

// ---------------------------------------------------------------------------
// helpers (also used by the catalog routes)
// ---------------------------------------------------------------------------

/// Upsert a seller row, changing only the provided fields.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_seller(
    st: &AppState,
    user_id: &str,
    display_name: Option<&str>,
    verified: Option<bool>,
    trusted: Option<bool>,
    frozen: Option<bool>,
    banned: Option<bool>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO sellers (user_id, display_name, verified, trusted, frozen, banned, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET
            display_name = COALESCE(?, display_name),
            verified = COALESCE(?, verified),
            trusted  = COALESCE(?, trusted),
            frozen   = COALESCE(?, frozen),
            banned   = COALESCE(?, banned),
            updated_at = ?",
    )
    .bind(user_id)
    .bind(display_name)
    .bind(verified.map(|b| b as i64).unwrap_or(0))
    .bind(trusted.map(|b| b as i64).unwrap_or(0))
    .bind(frozen.map(|b| b as i64).unwrap_or(0))
    .bind(banned.map(|b| b as i64).unwrap_or(0))
    .bind(now())
    .bind(now())
    // update side
    .bind(display_name)
    .bind(verified.map(|b| b as i64))
    .bind(trusted.map(|b| b as i64))
    .bind(frozen.map(|b| b as i64))
    .bind(banned.map(|b| b as i64))
    .bind(now())
    .execute(&st.pool)
    .await?;
    Ok(())
}

pub struct SellerFlags {
    pub verified: bool,
    pub trusted: bool,
    pub frozen: bool,
    pub banned: bool,
}

pub async fn load_flags(st: &AppState, user_id: &str) -> AppResult<SellerFlags> {
    let row = sqlx::query("SELECT verified, trusted, frozen, banned FROM sellers WHERE user_id=?")
        .bind(user_id)
        .fetch_optional(&st.pool)
        .await?;
    Ok(match row {
        Some(r) => SellerFlags {
            verified: r.get::<i64, _>("verified") != 0,
            trusted: r.get::<i64, _>("trusted") != 0,
            frozen: r.get::<i64, _>("frozen") != 0,
            banned: r.get::<i64, _>("banned") != 0,
        },
        None => SellerFlags {
            verified: false,
            trusted: false,
            frozen: false,
            banned: false,
        },
    })
}

async fn seller_badges(st: &AppState, user_id: &str) -> AppResult<Value> {
    let f = load_flags(st, user_id).await?;
    Ok(json!({
        "verified": f.verified,
        "trusted": f.trusted,
        "frozen": f.frozen,
        "banned": f.banned,
        "new_seller": !f.trusted,
        "price_cap_cents": if f.trusted { Value::Null } else { json!(NEW_SELLER_PRICE_CAP_CENTS) },
    }))
}
