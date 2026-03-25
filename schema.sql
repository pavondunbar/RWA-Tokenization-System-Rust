-- =============================================================
-- RWA-RUST Database Schema
-- Fully append-only: every table is protected by triggers that
-- reject UPDATE and DELETE at the database level.
-- =============================================================

CREATE TABLE rwa_assets (
  id UUID PRIMARY KEY,
  type VARCHAR(20) NOT NULL,
  name VARCHAR(256) NOT NULL,
  total_value DECIMAL(38,18) NOT NULL,
  jurisdiction VARCHAR(64) NOT NULL,
  custodian VARCHAR(256) NOT NULL,
  status VARCHAR(20) NOT NULL,
  idempotency_key VARCHAR(256) UNIQUE,
  created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE rwa_asset_status_events (
  id UUID PRIMARY KEY,
  asset_id UUID NOT NULL REFERENCES rwa_assets(id),
  status VARCHAR(20) NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_asset_status_latest
  ON rwa_asset_status_events(asset_id, created_at DESC);

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

CREATE TABLE compliance_status_events (
  id UUID PRIMARY KEY,
  compliance_id UUID NOT NULL REFERENCES investor_compliance(id),
  investor_id UUID NOT NULL,
  status VARCHAR(20) NOT NULL,
  reason TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_compliance_status_latest
  ON compliance_status_events(investor_id, created_at DESC);

CREATE TABLE wallet_whitelist_events (
  id UUID PRIMARY KEY,
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) NOT NULL,
  tier VARCHAR(20) NOT NULL,
  action VARCHAR(10) NOT NULL CHECK (action IN ('grant', 'revoke')),
  reason TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_whitelist_latest
  ON wallet_whitelist_events(wallet_address, created_at DESC);

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

CREATE TABLE token_mint_events (
  id UUID PRIMARY KEY,
  mint_id UUID NOT NULL REFERENCES token_mints(id),
  status VARCHAR(20) NOT NULL,
  tx_hash VARCHAR(256),
  block_number BIGINT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_mint_events_latest
  ON token_mint_events(mint_id, created_at DESC);

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

CREATE TABLE token_redemption_events (
  id UUID PRIMARY KEY,
  redemption_id UUID NOT NULL REFERENCES token_redemptions(id),
  status VARCHAR(20) NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_redemption_events_latest
  ON token_redemption_events(redemption_id, created_at DESC);

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
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE outbox_publish_log (
  id UUID PRIMARY KEY,
  event_id UUID NOT NULL UNIQUE REFERENCES outbox_events(id),
  published_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE ledger_entries (
  id UUID PRIMARY KEY,
  entry_date TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  event_type VARCHAR(64) NOT NULL,
  reference_id UUID NOT NULL,
  account VARCHAR(256) NOT NULL,
  asset_id UUID NOT NULL REFERENCES rwa_assets(id),
  debit DECIMAL(38,18) NOT NULL DEFAULT 0,
  credit DECIMAL(38,18) NOT NULL DEFAULT 0,
  currency VARCHAR(20) NOT NULL CHECK (currency IN ('TOKEN', 'USD')),
  memo TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  CONSTRAINT positive_amounts CHECK (debit >= 0 AND credit >= 0),
  CONSTRAINT single_side CHECK (
    (debit > 0 AND credit = 0) OR (debit = 0 AND credit > 0)
  )
);
CREATE INDEX idx_ledger_account ON ledger_entries(account, asset_id);
CREATE INDEX idx_ledger_reference ON ledger_entries(reference_id);

CREATE INDEX idx_token_mints_investor
ON token_mints(investor_id, asset_id, status);

-- =============================================================
-- APPEND-ONLY LEDGER PROTECTIONS
-- Enforce immutability at the database layer via triggers.
-- Every table is fully append-only: no UPDATE or DELETE allowed.
-- =============================================================

CREATE OR REPLACE FUNCTION reject_ledger_mutation()
RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'ledger table % is append-only: % denied',
    TG_TABLE_NAME, TG_OP;
END;
$$ LANGUAGE plpgsql;

-- rwa_assets: immutable after creation
CREATE TRIGGER rwa_assets_immutable
  BEFORE UPDATE OR DELETE ON rwa_assets
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- rwa_asset_status_events: append-only event log
CREATE TRIGGER rwa_asset_status_events_immutable
  BEFORE UPDATE OR DELETE ON rwa_asset_status_events
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- legal_wrappers: permanent legal structure
CREATE TRIGGER legal_wrappers_immutable
  BEFORE UPDATE OR DELETE ON legal_wrappers
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- investor_compliance: immutable compliance records
CREATE TRIGGER investor_compliance_immutable
  BEFORE UPDATE OR DELETE ON investor_compliance
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- compliance_status_events: append-only event log
CREATE TRIGGER compliance_status_events_immutable
  BEFORE UPDATE OR DELETE ON compliance_status_events
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- wallet_whitelist_events: append-only event log
CREATE TRIGGER wallet_whitelist_events_immutable
  BEFORE UPDATE OR DELETE ON wallet_whitelist_events
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- token_mints: fully immutable (no more conditional updates)
CREATE TRIGGER token_mints_immutable
  BEFORE UPDATE OR DELETE ON token_mints
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- token_mint_events: append-only event log
CREATE TRIGGER token_mint_events_immutable
  BEFORE UPDATE OR DELETE ON token_mint_events
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- token_redemptions: fully immutable (no more conditional updates)
CREATE TRIGGER token_redemptions_immutable
  BEFORE UPDATE OR DELETE ON token_redemptions
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- token_redemption_events: append-only event log
CREATE TRIGGER token_redemption_events_immutable
  BEFORE UPDATE OR DELETE ON token_redemption_events
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- nav_calculations: historical yield snapshots
CREATE TRIGGER nav_calculations_immutable
  BEFORE UPDATE OR DELETE ON nav_calculations
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- outbox_events: immutable event records
CREATE TRIGGER outbox_events_immutable
  BEFORE UPDATE OR DELETE ON outbox_events
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- outbox_publish_log: append-only publish tracking
CREATE TRIGGER outbox_publish_log_immutable
  BEFORE UPDATE OR DELETE ON outbox_publish_log
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- ledger_entries: double-entry accounting journal
CREATE TRIGGER ledger_entries_immutable
  BEFORE UPDATE OR DELETE ON ledger_entries
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();
