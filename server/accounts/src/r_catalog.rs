//! Catalog / store (spec §9) — browse + install FREE effects in-app, plus
//! owner-gated reviews. Logically `store.vance`; co-located to reuse the DB and
//! the M2 artifact/download machinery. Paid listings exist in the data model but
//! their purchase flow is M4 — the free-install endpoint refuses them.

use crate::auth::AuthUser;
use crate::crypto::{self, Claims, DownloadClaims};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::util::{new_id, now};
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

const DOWNLOAD_TTL_SECS: i64 = 300;
const DEFAULT_PAGE_SIZE: i64 = 24;

fn parse_json_array(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| json!([]))
}

// ---------------------------------------------------------------------------
// GET /v1/catalog  (public)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BrowseParams {
    q: Option<String>,
    category: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

pub async fn browse(
    State(st): State<AppState>,
    Query(p): Query<BrowseParams>,
) -> AppResult<Json<Value>> {
    let page = p.page.unwrap_or(0).max(0);
    let size = p.page_size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, 100);

    let mut sql = String::from(
        "SELECT c.sku, c.publisher, c.display_name, c.description, c.kind, c.categories, c.tags, c.price_cents, c.icon, c.downloads,
                (SELECT AVG(stars) FROM catalog_reviews r WHERE r.sku=c.sku) AS rating,
                (SELECT COUNT(*)   FROM catalog_reviews r WHERE r.sku=c.sku) AS ratings_count,
                (SELECT COUNT(*)   FROM reports rp WHERE rp.sku=c.sku) AS report_count,
                COALESCE(s.verified,0) AS seller_verified,
                COALESCE(s.trusted,0)  AS seller_trusted
         FROM catalog_effects c
         LEFT JOIN sellers s ON s.user_id = c.seller_id
         WHERE c.listed=1
           AND COALESCE(s.frozen,0)=0 AND COALESCE(s.banned,0)=0
           AND (c.price_cents=0 OR COALESCE(s.verified,0)=1)",
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(q) = p.q.as_ref().filter(|s| !s.trim().is_empty()) {
        sql.push_str(" AND (c.display_name LIKE ? OR c.description LIKE ? OR c.tags LIKE ?)");
        let like = format!("%{q}%");
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    if let Some(cat) = p.category.as_ref().filter(|s| !s.trim().is_empty()) {
        sql.push_str(" AND c.categories LIKE ?");
        binds.push(format!("%\"{cat}\"%"));
    }
    sql.push_str(match p.sort.as_deref() {
        Some("newest") => " ORDER BY c.created_at DESC",
        Some("name") => " ORDER BY c.display_name COLLATE NOCASE ASC",
        _ => " ORDER BY c.downloads DESC, c.display_name COLLATE NOCASE ASC",
    });
    sql.push_str(" LIMIT ? OFFSET ?");

    let mut query = sqlx::query(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    let rows = query
        .bind(size)
        .bind(page * size)
        .fetch_all(&st.pool)
        .await?;

    let effects: Vec<Value> = rows.iter().map(row_to_card).collect();
    Ok(Json(json!({
        "effects": effects,
        "page": page,
        "page_size": size,
        "has_more": effects.len() as i64 == size,
    })))
}

fn row_to_card(r: &sqlx::sqlite::SqliteRow) -> Value {
    let downloads: i64 = r.get("downloads");
    let reports: i64 = r.try_get("report_count").unwrap_or(0);
    let verified = r.try_get::<i64, _>("seller_verified").unwrap_or(0) != 0;
    let trusted = r.try_get::<i64, _>("seller_trusted").unwrap_or(0) != 0;
    let dispute_rate = if downloads > 0 {
        reports as f64 / downloads as f64
    } else {
        0.0
    };
    json!({
        "sku": r.get::<String,_>("sku"),
        "publisher": r.get::<String,_>("publisher"),
        "display_name": r.get::<String,_>("display_name"),
        "description": r.get::<Option<String>,_>("description"),
        "kind": r.get::<String,_>("kind"),
        "categories": parse_json_array(&r.get::<String,_>("categories")),
        "tags": parse_json_array(&r.get::<String,_>("tags")),
        "price_cents": r.get::<i64,_>("price_cents"),
        "icon": r.get::<Option<String>,_>("icon"),
        "downloads": downloads,
        "rating": r.get::<Option<f64>,_>("rating"),
        "ratings_count": r.get::<i64,_>("ratings_count"),
        // M5 — seller trust surfaced to buyers (spec §8a).
        "verified_seller": verified,
        "new_seller": !trusted,
        "reports": reports,
        "dispute_rate": dispute_rate,
    })
}

// ---------------------------------------------------------------------------
// GET /v1/catalog/:sku  (public)
// ---------------------------------------------------------------------------

pub async fn detail(State(st): State<AppState>, Path(sku): Path<String>) -> AppResult<Json<Value>> {
    let row = sqlx::query(
        "SELECT c.sku, c.publisher, c.display_name, c.description, c.kind, c.categories, c.tags, c.capabilities,
                c.price_cents, c.icon, c.downloads,
                (SELECT AVG(stars) FROM catalog_reviews r WHERE r.sku=c.sku) AS rating,
                (SELECT COUNT(*)   FROM catalog_reviews r WHERE r.sku=c.sku) AS ratings_count,
                (SELECT COUNT(*)   FROM reports rp WHERE rp.sku=c.sku) AS report_count,
                COALESCE(s.verified,0) AS seller_verified,
                COALESCE(s.trusted,0)  AS seller_trusted
         FROM catalog_effects c
         LEFT JOIN sellers s ON s.user_id = c.seller_id
         WHERE c.sku=? AND c.listed=1
           AND COALESCE(s.frozen,0)=0 AND COALESCE(s.banned,0)=0
           AND (c.price_cents=0 OR COALESCE(s.verified,0)=1)",
    )
    .bind(&sku)
    .fetch_optional(&st.pool)
    .await?
    .ok_or_else(|| AppError::NotFound("no such effect".into()))?;

    let mut card = row_to_card(&row);
    let art = sqlx::query("SELECT version, sha256, dest FROM plugin_artifacts WHERE sku=?")
        .bind(&sku)
        .fetch_optional(&st.pool)
        .await?;
    let obj = card.as_object_mut().unwrap();
    obj.insert(
        "capabilities".into(),
        parse_json_array(&row.get::<String, _>("capabilities")),
    );
    obj.insert(
        "installable".into(),
        json!(art.is_some() && row.get::<i64, _>("price_cents") == 0),
    );
    if let Some(a) = art {
        obj.insert("version".into(), json!(a.get::<String, _>("version")));
    }
    Ok(Json(card))
}

// ---------------------------------------------------------------------------
// GET /v1/catalog/:sku/reviews  (public)
// ---------------------------------------------------------------------------

pub async fn reviews(
    State(st): State<AppState>,
    Path(sku): Path<String>,
) -> AppResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT stars, body, created_at FROM catalog_reviews WHERE sku=? ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&sku)
    .fetch_all(&st.pool)
    .await?;
    let reviews: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "stars": r.get::<i64,_>("stars"),
                "body": r.get::<Option<String>,_>("body"),
                "created_at": r.get::<i64,_>("created_at"),
            })
        })
        .collect();
    let agg = sqlx::query("SELECT AVG(stars) AS rating, COUNT(*) AS count FROM catalog_reviews WHERE sku=?")
        .bind(&sku)
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(json!({
        "reviews": reviews,
        "rating": agg.get::<Option<f64>,_>("rating"),
        "count": agg.get::<i64,_>("count"),
    })))
}

// ---------------------------------------------------------------------------
// POST /v1/catalog/:sku/artifact  (public; FREE only) — optional bearer records
// the install so the user can later review.
// ---------------------------------------------------------------------------

pub async fn free_artifact(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(sku): Path<String>,
) -> AppResult<Json<Value>> {
    let listing = sqlx::query("SELECT price_cents FROM catalog_effects WHERE sku=? AND listed=1")
        .bind(&sku)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("no such effect".into()))?;
    if listing.get::<i64, _>("price_cents") != 0 {
        return Err(AppError::Forbidden(
            "this effect requires purchase (coming with payments)".into(),
        ));
    }
    let art = sqlx::query("SELECT version, platform, sha256, dest, requires_entitlement FROM plugin_artifacts WHERE sku=?")
        .bind(&sku)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("no downloadable file for that effect".into()))?;
    if art.get::<i64, _>("requires_entitlement") != 0 {
        return Err(AppError::Forbidden("that effect isn't free".into()));
    }
    let version: String = art.get("version");
    let platform: String = art.get("platform");
    let sha256: String = art.get("sha256");
    let dest: String = art.get("dest");

    // If the caller is signed in, record the install so reviews can be gated.
    if let Some(claims) = optional_user(&st, &headers).await {
        let _ = sqlx::query(
            "INSERT INTO device_installs (id, user_id, session_id, sku, installed_version, installed_at, last_seen_at)
             VALUES (?,?,?,?,?,?,?)
             ON CONFLICT(session_id, sku) DO UPDATE SET last_seen_at=excluded.last_seen_at, uninstalled_at=NULL",
        )
        .bind(new_id())
        .bind(&claims.sub)
        .bind(&claims.sid)
        .bind(&sku)
        .bind(&version)
        .bind(now())
        .bind(now())
        .execute(&st.pool)
        .await;
    }

    sqlx::query("UPDATE catalog_effects SET downloads=downloads+1 WHERE sku=?")
        .bind(&sku)
        .execute(&st.pool)
        .await?;

    let dl = DownloadClaims {
        sku: sku.clone(),
        platform,
        exp: now() + DOWNLOAD_TTL_SECS,
    };
    let token = crypto::sign_download(&st.signing, &dl);
    Ok(Json(json!({
        "download_url": format!("{}/download/{}", st.config.public_url, token),
        "sha256": sha256,
        "dest": dest,
        "version": version,
    })))
}

// ---------------------------------------------------------------------------
// POST /v1/catalog/:sku/reviews  (auth; owner-gated)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReviewBody {
    stars: i64,
    body: Option<String>,
}

pub async fn post_review(
    State(st): State<AppState>,
    user: AuthUser,
    Path(sku): Path<String>,
    Json(b): Json<ReviewBody>,
) -> AppResult<Json<Value>> {
    if !(1..=5).contains(&b.stars) {
        return Err(AppError::BadRequest("stars must be 1–5".into()));
    }
    // Gate: must own or have installed it (spec §9, kills drive-by spam).
    let owns = sqlx::query(
        "SELECT 1 FROM entitlements WHERE user_id=? AND sku=? AND state='active'
         UNION
         SELECT 1 FROM device_installs WHERE user_id=? AND sku=? AND uninstalled_at IS NULL",
    )
    .bind(&user.claims.sub)
    .bind(&sku)
    .bind(&user.claims.sub)
    .bind(&sku)
    .fetch_optional(&st.pool)
    .await?
    .is_some();
    if !owns {
        return Err(AppError::Forbidden("install this effect before reviewing it".into()));
    }

    sqlx::query(
        "INSERT INTO catalog_reviews (id, sku, user_id, stars, body, created_at)
         VALUES (?,?,?,?,?,?)
         ON CONFLICT(sku, user_id) DO UPDATE SET stars=excluded.stars, body=excluded.body, created_at=excluded.created_at",
    )
    .bind(new_id())
    .bind(&sku)
    .bind(&user.claims.sub)
    .bind(b.stars)
    .bind(&b.body)
    .bind(now())
    .execute(&st.pool)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// POST /v1/dev/catalog-publish  (dev only) — list a FREE effect for testing.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CatalogPublishBody {
    sku: String,
    file_path: String,
    dest: String,
    publisher: String,
    display_name: String,
    description: Option<String>,
    kind: Option<String>,
    categories: Option<Vec<String>>,
    tags: Option<Vec<String>>,
    capabilities: Option<Vec<String>>,
    /// > 0 → a PAID (encrypted) listing; needs a verified seller (spec §8a).
    price_cents: Option<i64>,
}

pub async fn dev_catalog_publish(
    State(st): State<AppState>,
    user: AuthUser,
    Json(p): Json<CatalogPublishBody>,
) -> AppResult<Json<Value>> {
    if !st.config.dev_provider {
        return Err(AppError::Forbidden("dev endpoints are disabled".into()));
    }
    let price = p.price_cents.unwrap_or(0).max(0);
    let seller_id = user.claims.sub.clone();

    // Ensure a seller row exists; enforce the paid-listing trust rules (§8a).
    crate::r_sellers::upsert_seller(&st, &seller_id, Some(&p.publisher), None, None, None, None).await?;
    let flags = crate::r_sellers::load_flags(&st, &seller_id).await?;
    if price > 0 {
        if !flags.verified {
            return Err(AppError::Forbidden(
                "get verified (connect payouts) before selling paid effects".into(),
            ));
        }
        if !flags.trusted && price > crate::r_sellers::NEW_SELLER_PRICE_CAP_CENTS {
            return Err(AppError::BadRequest(format!(
                "new sellers can list up to ${:.0} until they've earned trust",
                crate::r_sellers::NEW_SELLER_PRICE_CAP_CENTS as f64 / 100.0
            )));
        }
    }

    let plaintext = tokio::fs::read(&p.file_path)
        .await
        .map_err(|e| AppError::BadRequest(format!("can't read {}: {e}", p.file_path)))?;

    // Paid → encrypt at rest (spec §7); free → plaintext.
    if price > 0 {
        let plaintext_sha = crypto::sha256_b64(&plaintext);
        let (cek, ciphertext) = crypto::encrypt_effect(&plaintext);
        let ct_path = format!("{}.enc", p.file_path);
        tokio::fs::write(&ct_path, &ciphertext)
            .await
            .map_err(|e| AppError::Internal(format!("write ciphertext: {e}")))?;
        let pin = crypto::sha256_b64(&ciphertext);
        sqlx::query(
            "INSERT INTO plugin_artifacts (sku, version, platform, object_key, sha256, dest, requires_entitlement, display_name, encrypted, cek, plaintext_sha256)
             VALUES (?, '1', 'any', ?, ?, ?, 1, ?, 1, ?, ?)
             ON CONFLICT(sku) DO UPDATE SET object_key=excluded.object_key, sha256=excluded.sha256, dest=excluded.dest,
                requires_entitlement=1, display_name=excluded.display_name, encrypted=1, cek=excluded.cek, plaintext_sha256=excluded.plaintext_sha256",
        )
        .bind(&p.sku).bind(&ct_path).bind(&pin).bind(&p.dest).bind(&p.display_name)
        .bind(crypto::b64(&cek)).bind(&plaintext_sha)
        .execute(&st.pool).await?;
    } else {
        let sha = crypto::sha256_b64(&plaintext);
        sqlx::query(
            "INSERT INTO plugin_artifacts (sku, version, platform, object_key, sha256, dest, requires_entitlement, display_name, encrypted)
             VALUES (?, '1', 'any', ?, ?, ?, 0, ?, 0)
             ON CONFLICT(sku) DO UPDATE SET object_key=excluded.object_key, sha256=excluded.sha256,
                dest=excluded.dest, requires_entitlement=0, display_name=excluded.display_name, encrypted=0",
        )
        .bind(&p.sku).bind(&p.file_path).bind(&sha).bind(&p.dest).bind(&p.display_name)
        .execute(&st.pool).await?;
    }

    let kind = p.kind.unwrap_or_else(|| "effect".into());
    let cats = serde_json::to_string(&p.categories.unwrap_or_default()).unwrap();
    let tags = serde_json::to_string(&p.tags.unwrap_or_default()).unwrap();
    let caps = serde_json::to_string(&p.capabilities.unwrap_or_default()).unwrap();
    // The connected account to charge (the seller's), if they've connected one.
    let stripe_account: Option<String> =
        sqlx::query("SELECT account_id FROM stripe_connected_accounts WHERE user_id=?")
            .bind(&seller_id)
            .fetch_optional(&st.pool)
            .await?
            .map(|r| r.get::<String, _>("account_id"));

    sqlx::query(
        "INSERT INTO catalog_effects
            (sku, publisher, display_name, description, kind, categories, tags, capabilities, price_cents, listed, created_at, updated_at, seller_id, stripe_account)
         VALUES (?,?,?,?,?,?,?,?,?,1,?,?,?,?)
         ON CONFLICT(sku) DO UPDATE SET publisher=excluded.publisher, display_name=excluded.display_name,
            description=excluded.description, kind=excluded.kind, categories=excluded.categories,
            tags=excluded.tags, capabilities=excluded.capabilities, price_cents=excluded.price_cents,
            updated_at=excluded.updated_at, seller_id=excluded.seller_id, stripe_account=excluded.stripe_account",
    )
    .bind(&p.sku)
    .bind(&p.publisher)
    .bind(&p.display_name)
    .bind(&p.description)
    .bind(&kind)
    .bind(&cats)
    .bind(&tags)
    .bind(&caps)
    .bind(price)
    .bind(now())
    .bind(now())
    .bind(&seller_id)
    .bind(&stripe_account)
    .execute(&st.pool)
    .await?;

    Ok(Json(json!({ "sku": p.sku, "price_cents": price, "encrypted": price > 0, "listed": true })))
}

// ---------------------------------------------------------------------------
// helper: parse + verify an OPTIONAL bearer (returns None rather than erroring).
// ---------------------------------------------------------------------------

async fn optional_user(st: &AppState, headers: &HeaderMap) -> Option<Claims> {
    let token = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let claims = crypto::verify_access(&st.verifying, token)?;
    if claims.exp < now() {
        return None;
    }
    let row = sqlx::query("SELECT revoked_at FROM sessions WHERE id=?")
        .bind(&claims.sid)
        .fetch_optional(&st.pool)
        .await
        .ok()??;
    if row.try_get::<Option<i64>, _>("revoked_at").ok()?.is_some() {
        return None;
    }
    Some(claims)
}
