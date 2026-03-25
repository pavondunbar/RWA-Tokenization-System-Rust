--- SAMPLE DATA: BLACKROCK'S BUIDL TOKEN --

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- -----------------------------------------------
-- SCENARIO: Blackstone Treasury Token Fund (BTTF)
-- Tokenized US Treasury Bill Fund on Ethereum
-- $500M total value | $1.00 per token | 500M tokens
-- Institutional investors only | Delaware Trust
-- -----------------------------------------------

-- STEP 1: Register the real world asset
INSERT INTO rwa_assets (
    id, type, name, total_value,
    jurisdiction, custodian, status,
    idempotency_key, created_at
)
VALUES (
    gen_random_uuid(),
    'treasury_bond',
    'Blackstone Treasury Token Fund (BTTF)',
    500000000.000000000000000000,   -- $500M total value
    'Delaware, United States',
    'BNY Mellon Digital Assets',    -- regulated custodian
    'approved',
    gen_random_uuid()::VARCHAR,
    NOW()
);

-- Record initial status event
INSERT INTO rwa_asset_status_events (id, asset_id, status)
SELECT gen_random_uuid(), id, 'approved'
FROM rwa_assets
WHERE name = 'Blackstone Treasury Token Fund (BTTF)';

SELECT * FROM rwa_assets;

-- -----------------------------------------------
-- STEP 2: Create the legal wrapper
-- Delaware Trust — holds T-bills, issues token shares
-- 500M tokens at $1.00 each = $500M NAV
-- -----------------------------------------------

INSERT INTO legal_wrappers (
    id, asset_id, structure_type,
    jurisdiction, token_supply,
    price_per_token, status, created_at
)
SELECT
    gen_random_uuid(),
    id,                                         -- references rwa_assets.id
    'TRUST',                                    -- Delaware Trust structure
    'Delaware, United States',
    500000000.000000000000000000,               -- 500 million tokens
    1.000000000000000000,                       -- $1.00 per token
    'active',
    NOW()
FROM rwa_assets
WHERE name = 'Blackstone Treasury Token Fund (BTTF)';

SELECT * FROM legal_wrappers;

-- -----------------------------------------------
-- STEP 3: Onboard three institutional investors
-- Investor 1: Citadel Securities       — $50M
-- Investor 2: Fidelity Investments     — $25M
-- Investor 3: Goldman Sachs Asset Mgmt — $10M
-- -----------------------------------------------

INSERT INTO investor_compliance (
    id, investor_id, wallet_address,
    tier, jurisdiction, status,
    kyc_reference, idempotency_key,
    created_at, expires_at
)
VALUES
(
    gen_random_uuid(),
    gen_random_uuid(),                          -- Citadel investor_id
    '0xCitadel1A2B3C4D5E6F7890abcdef123456',   -- Citadel ETH wallet
    'institutional',
    'Illinois, United States',
    'approved',
    'KYC-SECURITIZE-CITADEL-2024-001',         -- Securitize KYC ref
    gen_random_uuid()::VARCHAR,
    NOW(),
    NOW() + INTERVAL '1 year'                  -- KYC expires in 1 year
),
(
    gen_random_uuid(),
    gen_random_uuid(),                          -- Fidelity investor_id
    '0xFidelity9A8B7C6D5E4F3210fedcba654321',  -- Fidelity ETH wallet
    'institutional',
    'Massachusetts, United States',
    'approved',
    'KYC-SECURITIZE-FIDELITY-2024-002',
    gen_random_uuid()::VARCHAR,
    NOW(),
    NOW() + INTERVAL '1 year'
),
(
    gen_random_uuid(),
    gen_random_uuid(),                          -- Goldman investor_id
    '0xGoldman2C3D4E5F6A7B8901234567890abc',   -- Goldman ETH wallet
    'institutional',
    'New York, United States',
    'approved',
    'KYC-SECURITIZE-GOLDMAN-2024-003',
    gen_random_uuid()::VARCHAR,
    NOW(),
    NOW() + INTERVAL '1 year'
);

SELECT * FROM investor_compliance;

-- -----------------------------------------------
-- STEP 4: Whitelist all three investor wallets
-- via append-only wallet_whitelist_events
-- -----------------------------------------------

INSERT INTO wallet_whitelist_events (
    id, investor_id, wallet_address, tier, action
)
SELECT
    gen_random_uuid(),
    investor_id,
    wallet_address,
    tier,
    'grant'
FROM investor_compliance
WHERE status = 'approved';

SELECT * FROM wallet_whitelist_events;

-- -----------------------------------------------
-- STEP 5: Mint tokens to each investor
-- Citadel  wired $50M  → receives 50,000,000 BTTF tokens
-- Fidelity wired $25M  → receives 25,000,000 BTTF tokens
-- Goldman  wired $10M  → receives 10,000,000 BTTF tokens
-- -----------------------------------------------

INSERT INTO token_mints (
    id, asset_id, investor_id,
    wallet_address, token_amount,
    fiat_received, status,
    idempotency_key, created_at,
    confirmed_at
)
SELECT
    gen_random_uuid(),
    a.id,
    c.investor_id,
    c.wallet_address,
    CASE c.wallet_address
        WHEN '0xCitadel1A2B3C4D5E6F7890abcdef123456'
            THEN 50000000.000000000000000000    -- 50M tokens
        WHEN '0xFidelity9A8B7C6D5E4F3210fedcba654321'
            THEN 25000000.000000000000000000    -- 25M tokens
        WHEN '0xGoldman2C3D4E5F6A7B8901234567890abc'
            THEN 10000000.000000000000000000    -- 10M tokens
    END,
    CASE c.wallet_address
        WHEN '0xCitadel1A2B3C4D5E6F7890abcdef123456'
            THEN 50000000.000000000000000000    -- $50M wired
        WHEN '0xFidelity9A8B7C6D5E4F3210fedcba654321'
            THEN 25000000.000000000000000000    -- $25M wired
        WHEN '0xGoldman2C3D4E5F6A7B8901234567890abc'
            THEN 10000000.000000000000000000    -- $10M wired
    END,
    'confirmed',
    gen_random_uuid()::VARCHAR,
    NOW(),
    NOW()                                       -- confirmed on-chain
FROM investor_compliance c
CROSS JOIN rwa_assets a
WHERE a.name = 'Blackstone Treasury Token Fund (BTTF)'
AND c.status = 'approved';

-- Record confirmation events for each mint
INSERT INTO token_mint_events (id, mint_id, status)
SELECT gen_random_uuid(), id, 'confirmed'
FROM token_mints
WHERE status = 'confirmed';

SELECT * FROM token_mints;

-- -----------------------------------------------
-- STEP 6: Full verification — the complete picture
-- -----------------------------------------------

-- Fund overview
SELECT
    a.name                              AS fund_name,
    a.total_value                       AS fund_total_value,
    a.custodian,
    a.jurisdiction,
    lw.structure_type                   AS legal_structure,
    lw.token_supply                     AS total_tokens,
    lw.price_per_token                  AS token_price
FROM rwa_assets a
JOIN legal_wrappers lw ON lw.asset_id = a.id;

-- Investor positions
SELECT
    c.wallet_address,
    c.tier,
    c.jurisdiction,
    c.status                            AS kyc_status,
    c.expires_at                        AS kyc_expires,
    tm.token_amount                     AS tokens_held,
    tm.fiat_received                    AS usd_invested,
    tm.status                           AS mint_status,
    tm.confirmed_at                     AS confirmed_on_chain
FROM investor_compliance c
JOIN token_mints tm ON tm.investor_id = c.investor_id
ORDER BY tm.token_amount DESC;

-- Fund summary — tokens minted vs total supply
SELECT
    a.name                              AS fund,
    lw.token_supply                     AS total_supply,
    SUM(tm.token_amount)                AS tokens_minted,
    lw.token_supply - SUM(tm.token_amount) AS tokens_remaining,
    SUM(tm.fiat_received)               AS total_usd_raised
FROM rwa_assets a
JOIN legal_wrappers lw ON lw.asset_id = a.id
JOIN token_mints tm ON tm.asset_id = a.id
WHERE tm.status = 'confirmed'
GROUP BY a.name, lw.token_supply;

-- -----------------------------------------------
-- VERIFICATION — Complete Fund Picture
-- -----------------------------------------------

-- 1. Fund overview
SELECT
    a.name                              AS fund_name,
    a.total_value                       AS total_value,
    a.custodian,
    lw.structure_type                   AS structure,
    lw.token_supply                     AS total_tokens,
    lw.price_per_token                  AS token_price
FROM rwa_assets a
JOIN legal_wrappers lw ON lw.asset_id = a.id;

-- 2. Investor positions
SELECT
    ic.wallet_address,
    ic.tier,
    ic.jurisdiction,
    tm.token_amount                     AS tokens_held,
    tm.fiat_received                    AS usd_invested,
    tm.status,
    tm.confirmed_at
FROM investor_compliance ic
JOIN token_mints tm
    ON tm.investor_id = ic.investor_id
ORDER BY tm.token_amount DESC;

-- 3. Fund summary
SELECT
    a.name                              AS fund,
    lw.token_supply                     AS total_supply,
    SUM(tm.token_amount)                AS tokens_minted,
    lw.token_supply - SUM(tm.token_amount)
                                        AS tokens_remaining,
    SUM(tm.fiat_received)               AS total_usd_raised,
    ROUND(
        (SUM(tm.token_amount) /
         lw.token_supply) * 100, 2
    )                                   AS percent_subscribed
FROM rwa_assets a
JOIN legal_wrappers lw ON lw.asset_id = a.id
JOIN token_mints tm ON tm.asset_id = a.id
WHERE tm.status = 'confirmed'
GROUP BY a.name, lw.token_supply;
