CREATE TABLE rwa_assets (
  id UUID PRIMARY KEY,
  type VARCHAR(20) NOT NULL,
  name VARCHAR(256) NOT NULL,
  total_value DECIMAL(38,18) NOT NULL,
  jurisdiction VARCHAR(64) NOT NULL,
  custodian VARCHAR(256) NOT NULL,
  status VARCHAR(20) NOT NULL,
  idempotency_key VARCHAR(256) UNIQUE,
  created_at TIMESTAMP NOT NULL,
  updated_at TIMESTAMP NOT NULL
);

CREATE TABLE legal_wrappers (
  id UUID PRIMARY KEY,
  asset_id UUID REFERENCES rwa_assets(id),
  structure_type VARCHAR(20) NOT NULL,  -- SPV, TRUST, FUND
  jurisdiction VARCHAR(64) NOT NULL,
  token_supply DECIMAL(38,18) NOT NULL,
  price_per_token DECIMAL(38,18) NOT NULL,
  status VARCHAR(20) NOT NULL,
  created_at TIMESTAMP NOT NULL
);

CREATE TABLE investor_compliance (
  id UUID PRIMARY KEY,
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) NOT NULL,
  tier VARCHAR(20) NOT NULL,            -- retail, accredited, institutional
  jurisdiction VARCHAR(64) NOT NULL,
  status VARCHAR(20) NOT NULL,
  kyc_reference VARCHAR(256),
  idempotency_key VARCHAR(256) UNIQUE,
  created_at TIMESTAMP NOT NULL,
  expires_at TIMESTAMP NOT NULL         -- KYC expires, must re-verify
);

CREATE TABLE whitelisted_wallets (
  id UUID PRIMARY KEY,
  investor_id UUID NOT NULL,
  wallet_address VARCHAR(256) UNIQUE NOT NULL,
  tier VARCHAR(20) NOT NULL,
  created_at TIMESTAMP NOT NULL
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
  created_at TIMESTAMP NOT NULL,
  confirmed_at TIMESTAMP               -- NULL until on-chain confirmation
);
