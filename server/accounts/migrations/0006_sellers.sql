-- Vance accounts — M5: seller trust + anti-scam (spec §8a). Take-and-run is
-- already designed out (M4 auto-activation); this hardens ordinary marketplace
-- risks: bad/mis-described effects, impersonation, review spam.

CREATE TABLE IF NOT EXISTS sellers (
    user_id      TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    display_name TEXT,
    verified     INTEGER NOT NULL DEFAULT 0, -- identity confirmed (Stripe KYC / dev)
    trusted      INTEGER NOT NULL DEFAULT 0, -- earned a track record (lifts new-seller caps)
    frozen       INTEGER NOT NULL DEFAULT 0, -- listings hidden pending review
    banned       INTEGER NOT NULL DEFAULT 0, -- permanent; identity blocked
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

-- One report per (user, effect); the aggregate drives the visible dispute rate
-- and auto-freeze.
CREATE TABLE IF NOT EXISTS reports (
    id               TEXT PRIMARY KEY,
    sku              TEXT NOT NULL,
    reporter_user_id TEXT NOT NULL,
    reason           TEXT,
    created_at       INTEGER NOT NULL,
    UNIQUE (sku, reporter_user_id)
);
CREATE INDEX IF NOT EXISTS idx_reports_sku ON reports(sku);

-- Which seller published a listing (for trust joins + the dispute rate).
ALTER TABLE catalog_effects ADD COLUMN seller_id TEXT;
CREATE INDEX IF NOT EXISTS idx_catalog_seller ON catalog_effects(seller_id);
