CREATE TABLE rwa_assets (
  id UUID PRIMARY KEY,
  type VARCHAR(20) NOT NULL,
  name VARCHAR(256) NOT NULL,
  total_value DECIMAL(38,18) NOT NULL,
  jurisdiction VARCHAR(64) NOT NULL,
  custodian VARCHAR(256) NOT NULL,
  status VARCHAR(20) NOT NULL,
  idempotency_key VARCHAR(256) UNIQUE,
  created_at TIMESTAMPTZ NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE legal_wrappers (
  id UUID PRIMARY KEY,
  asset_id UUID REFERENCES rwa_assets(id),
  structure_type VARCHAR(20) NOT NULL,
  jurisdiction VARCHAR(64) NOT NULL,
  token_supply DECIMAL(38,18) NOT NULL,
  price_per_token DECIMAL(38,18) NOT NULL,
  status VARCHAR(20) NOT NULL,
  created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE investor_compliance (
  id UUID PRIMARY KEY,
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) NOT NULL,
  tier VARCHAR(20) NOT NULL,
  jurisdiction VARCHAR(64) NOT NULL,
  status VARCHAR(20) NOT NULL,
  kyc_reference VARCHAR(256),
  idempotency_key VARCHAR(256) UNIQUE,
  created_at TIMESTAMPTZ NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE whitelisted_wallets (
  id UUID PRIMARY KEY,
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) UNIQUE NOT NULL,
  tier VARCHAR(20) NOT NULL,
  created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE token_mints (
  id UUID PRIMARY KEY,
  asset_id UUID REFERENCES rwa_assets(id),
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) NOT NULL,
  token_amount DECIMAL(38,18) NOT NULL,
  fiat_received DECIMAL(38,18) NOT NULL,
  status VARCHAR(20) NOT NULL,
  idempotency_key VARCHAR(256) UNIQUE,
  tx_hash VARCHAR(256),
  block_number BIGINT,
  created_at TIMESTAMPTZ NOT NULL,
  confirmed_at TIMESTAMPTZ
);

CREATE TABLE token_redemptions (
  id UUID PRIMARY KEY,
  asset_id UUID REFERENCES rwa_assets(id),
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) NOT NULL,
  token_amount DECIMAL(38,18) NOT NULL,
  bank_account VARCHAR(256) NOT NULL,
  status VARCHAR(20) NOT NULL,
  idempotency_key VARCHAR(256) UNIQUE,
  created_at TIMESTAMPTZ NOT NULL,
  settled_at TIMESTAMPTZ
);

CREATE TABLE nav_calculations (
  id UUID PRIMARY KEY,
  asset_id UUID REFERENCES rwa_assets(id),
  total_value DECIMAL(38,18) NOT NULL,
  daily_yield DECIMAL(38,18) NOT NULL,
  yield_rate DECIMAL(38,18) NOT NULL,
  calculated_at TIMESTAMPTZ NOT NULL,
  idempotency_key VARCHAR(256) UNIQUE
);

CREATE TABLE outbox_events (
  id UUID PRIMARY KEY,
  aggregate_id VARCHAR(256) NOT NULL,
  event_type VARCHAR(64) NOT NULL,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  published_at TIMESTAMPTZ
);

CREATE INDEX idx_outbox_unpublished
ON outbox_events(created_at)
WHERE published_at IS NULL;

CREATE INDEX idx_token_mints_investor
ON token_mints(investor_id, asset_id, status);

CREATE INDEX idx_whitelisted_wallets_address
ON whitelisted_wallets(wallet_address);

-- =============================================================
-- APPEND-ONLY LEDGER PROTECTIONS
-- Enforce immutability at the database layer via triggers.
-- =============================================================

-- Generic trigger function for fully append-only tables.
-- Rejects any UPDATE or DELETE unconditionally.
CREATE OR REPLACE FUNCTION reject_ledger_mutation()
RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'ledger table % is append-only: % denied',
    TG_TABLE_NAME, TG_OP;
END;
$$ LANGUAGE plpgsql;

-- legal_wrappers: permanent legal structure, never modified
CREATE TRIGGER legal_wrappers_immutable
  BEFORE UPDATE OR DELETE ON legal_wrappers
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- nav_calculations: historical yield snapshots, never modified
CREATE TRIGGER nav_calculations_immutable
  BEFORE UPDATE OR DELETE ON nav_calculations
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- token_mints: conditionally immutable after confirmation.
-- DELETE is always blocked. UPDATE is blocked once status = 'confirmed'.
CREATE OR REPLACE FUNCTION reject_settled_mint_mutation()
RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    RAISE EXCEPTION
      'token_mints: DELETE denied (append-only ledger)';
  END IF;
  IF OLD.status = 'confirmed' THEN
    RAISE EXCEPTION
      'token_mints: UPDATE denied (mint % is confirmed)',
      OLD.id;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER token_mints_immutable_after_confirm
  BEFORE UPDATE OR DELETE ON token_mints
  FOR EACH ROW EXECUTE FUNCTION reject_settled_mint_mutation();

-- token_redemptions: conditionally immutable after settlement.
-- DELETE is always blocked. UPDATE is blocked once status = 'settled'.
CREATE OR REPLACE FUNCTION reject_settled_redemption_mutation()
RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    RAISE EXCEPTION
      'token_redemptions: DELETE denied (append-only ledger)';
  END IF;
  IF OLD.status = 'settled' THEN
    RAISE EXCEPTION
      'token_redemptions: UPDATE denied (redemption % is settled)',
      OLD.id;
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER token_redemptions_immutable_after_settle
  BEFORE UPDATE OR DELETE ON token_redemptions
  FOR EACH ROW EXECUTE FUNCTION reject_settled_redemption_mutation();
