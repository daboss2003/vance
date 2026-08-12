-- Vance accounts — M4b: Stripe Connect payments (spec §8, §8a.1). Vance never
-- holds buyer funds: the buyer pays on the SELLER's connected account (direct
-- charge), Vance takes a 20% application fee, and the payment.succeeded webhook
-- auto-grants the effect.

-- Idempotency for webhook events (Stripe re-delivers).
CREATE TABLE IF NOT EXISTS stripe_events (
    event_id    TEXT PRIMARY KEY,
    kind        TEXT,
    received_at INTEGER NOT NULL
);

-- A seller's connected Stripe account (Standard/direct-charge).
CREATE TABLE IF NOT EXISTS stripe_connected_accounts (
    user_id         TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    account_id      TEXT NOT NULL,
    charges_enabled INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

-- The connected account a paid effect charges on (the seller's).
ALTER TABLE catalog_effects ADD COLUMN stripe_account TEXT;
