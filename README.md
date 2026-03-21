# RWA Tokenization Platform

> **DEMO ONLY — NOT FOR PRODUCTION USE**
> This codebase is a simplified demonstration of real-world asset tokenization architecture concepts.
> It is not audited, not hardened, and is not suitable for handling real funds, securities, or investor data under any circumstances.
> See [Production Considerations](#production-considerations) for a full list of what would need to change.

---

## Overview

A Rust implementation of a Real-World Asset (RWA) tokenization platform backed by PostgreSQL. It demonstrates how to bring traditional financial assets — treasury bonds, real estate, private equity, funds — onto a blockchain as regulated, permissioned security tokens.

Modelled after BlackRock's BUIDL token (the world's largest tokenized fund at $2.9B+), this platform covers the full asset lifecycle:

```
Asset Registration → Legal Wrapper → KYC/AML Onboarding → Token Minting
    → Daily NAV & Yield → Reconciliation → Token Redemption → Fiat Settlement
```

RWA tokens are fundamentally different from standard ERC-20 tokens. Every participant must pass KYC/AML. Every wallet must be whitelisted. Every transfer checks both sender and receiver. These constraints are enforced at the application layer and optionally mirrored on-chain via transfer hooks or oracles.

---

## What It Models

### BlackRock BUIDL — The Reference Implementation

The demo scenario uses a fictional `Blackstone Treasury Token Fund (BTTF)` that mirrors the BUIDL structure:

| Property | Value |
|---|---|
| Asset type | US Treasury Bills |
| Total value | $500M |
| Token supply | 500,000,000 tokens |
| Price per token | $1.00 (fixed NAV) |
| Yield mechanism | Positive daily rebase (~5% APY) |
| Legal structure | Delaware Trust |
| Custodian | BNY Mellon Digital Assets |
| Investor tier | Institutional only |
| KYC provider | Securitize (stubbed) |

Yield accrues by minting new tokens proportionally to each holder daily — the token price stays $1.00 and the investor's balance increases. This is called a **positive rebase**.

---

## Key Patterns Demonstrated

### Pessimistic Locking (`FOR UPDATE`)
Every operation that touches shared state acquires a row-level lock before reading. Token minting locks `legal_wrappers` to prevent two simultaneous $250M mints from both passing a $300M supply check. Legal wrapper creation locks `rwa_assets` to enforce one wrapper per asset. All locks are held for the minimum possible duration.

### Derived State (No Mutable Counters)
Token supply and investor balances are **derived** from append-only ledger tables, not stored in mutable counters. Allocated supply = `SUM(token_mints) - SUM(settled token_redemptions)`. This eliminates counter-consistency bugs and enables point-in-time audit queries. The total token supply is read from the immutable `legal_wrappers` row.

### Transactional Outbox
Every state-changing operation writes an outbox event in the **same database transaction** as the state change itself. If Kafka is down, the event waits safely in Postgres. A separate outbox poller delivers it once the broker recovers. There is no dual-write inconsistency and no lost events.

### Append-Only Ledger Protections
PostgreSQL triggers enforce immutability at the database layer. `legal_wrappers` and `nav_calculations` reject all UPDATE and DELETE operations unconditionally. `token_mints` and `token_redemptions` are conditionally immutable — updates are blocked once a mint is confirmed or a redemption is settled. Even application-layer bugs cannot corrupt finalized ledger records.

### Idempotency Keys
Every write operation accepts a caller-supplied `idempotency_key` and checks it before any writes. Safe to retry on network failures, process crashes, or timeout ambiguity — the second call returns the first call's result.

### State Machine Guards
Every service enforces valid state transitions before writing. `LegalWrapperService` requires `PENDING_LEGAL` before creating a wrapper. `TokenRedemptionService.settle_redemption()` requires `BURNED` before settling fiat. Invalid transitions return a typed `InvalidState` error with both the expected and actual state.

### Sanctions and KYC Outside Transactions
Both the OFAC/EU/UN sanctions check and the KYC/AML verification happen **before** any database transaction is opened. Holding a `FOR UPDATE` lock while waiting on an external compliance API would block all concurrent operations on that investor — never acceptable in a financial system.

### Hexagonal Architecture (Ports & Adapters)
Every external dependency — KYC provider, sanctions checker, custodian API, blockchain node, signing queue, Kafka, alert service — is behind an `async_trait` interface in `ports.rs`. The core services depend only on these traits. Swapping a stub for a real implementation (or mocking in tests) requires zero changes to the service logic.

### `FOR UPDATE SKIP LOCKED` Outbox
The outbox publisher uses `FOR UPDATE SKIP LOCKED`, allowing multiple publisher instances to run concurrently without a distributed lock or a coordinator. Each instance atomically claims a non-overlapping batch of rows, enabling horizontal scaling of event delivery.

### Typed Errors
Every failure mode is a distinct enum variant in `RwaError`, carrying the structured context needed to debug it — `InsufficientTokenSupply { available, requested }`, `InvalidState { expected, got }`, `ReconciliationMismatch { asset_id, details }`. No string parsing, no stringly-typed exceptions.

### Structured Logging
`tracing` and `#[instrument]` attach `asset_id`, `investor_id`, `token_amount` and other fields as structured log data on every line emitted inside a request, without manual threading.

---

## Project Structure

```
rwa/
├── Cargo.toml
├── schema.sql              # DDL + append-only ledger triggers
├── seed_test.sql           # Sample data (3 institutional investors)
└── src/
    ├── main.rs             # Entrypoint + all stub adapter implementations + demo scenario
    ├── errors.rs           # Typed error enum — one variant per failure mode
    ├── models.rs           # Enums, DB row types, request/response types, signing messages
    ├── ports.rs            # All external service trait boundaries
    ├── registry.rs         # RWA asset registration
    ├── legal.rs            # Legal wrapper creation (SPV, Trust, LLC, Fund)
    ├── compliance.rs       # KYC/AML onboarding, wallet whitelist, transfer gating
    ├── minting.rs          # Token minting — supply lock, on-chain confirmation
    ├── redemption.rs       # Token redemption — burn, balance lock, fiat settlement
    ├── nav.rs              # Daily NAV calculation and positive yield rebase
    ├── reconciliation.rs   # Ledger vs on-chain vs custodian cross-check
    └── outbox.rs           # Transactional outbox publisher (FOR UPDATE SKIP LOCKED)
```

---

## Module Responsibilities

**`registry.rs`** — Registers a real-world asset with its type, total value, jurisdiction, and custodian. Validates the custodian is approved. Writes a `rwa.registered` outbox event to trigger the downstream legal review workflow.

**`legal.rs`** — Creates the legal entity (SPV, Trust, LLC, or regulated Fund) that legally owns the asset. Computes `price_per_token = total_value / token_supply`. Records the token supply and price in `legal_wrappers` (immutable after creation). Advances the asset state machine from `PENDING_LEGAL` → `PENDING_AUDIT`.

**`compliance.rs`** — Full KYC/AML onboarding flow: sanctions screening → identity verification → atomic compliance record + wallet whitelist write. Also exposes `can_transfer(from, to)` — called before every token transfer to check both wallets. Supports whitelist revocation for expired KYC or sanctions hits.

**`minting.rs`** — Mints tokens to a whitelisted investor wallet on receipt of fiat. Locks `legal_wrappers` with `FOR UPDATE` and derives allocated supply from `token_mints` and `token_redemptions` to prevent overselling. Creates the mint record, writes the outbox event, and publishes to the signing queue — all in the right order with the right transaction boundaries. Handles on-chain confirmation via `confirm_mint()`.

**`redemption.rs`** — Inverse of minting. Verifies whitelist, locks the investor's confirmed token balance, creates the redemption record, and enqueues the on-chain burn. `settle_redemption()` is called after the burn is confirmed — it advances the redemption to `SETTLED`, which implicitly releases supply via the derived calculation, and triggers the fiat wire via outbox.

**`nav.rs`** — Calculates `daily_yield = total_value × (annual_rate / 365)` and accrues it to the fund's total value. Writes a `nav.yield_calculated` outbox event that triggers the on-chain rebase minter. Idempotency key format `asset_id:YYYY-MM-DD` ensures exactly one NAV calculation per asset per day.

**`reconciliation.rs`** — Derives allocated supply from ledger tables (`SUM(token_mints) - SUM(settled token_redemptions)`) and compares it against `totalSupply()` on-chain. Also compares `total_value` in the DB against the custodian's NAV report. Any mismatch fires a `CRITICAL` alert and halts new mints and redemptions. NAV comparison uses a $0.01 tolerance.

**`outbox.rs`** — Polls `outbox_events WHERE published_at IS NULL`, sends each event to the appropriate Kafka topic (`rwa.{event_type}`), and marks each event published individually inside the loop. `FOR UPDATE SKIP LOCKED` enables multiple concurrent publisher instances.

---

## Running in CoderPad

### 1. Set up the database schema

Open the **PostgreSQL panel** in CoderPad and run the full DDL from `schema.sql`:

```sql
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

-- Append-only ledger triggers

CREATE OR REPLACE FUNCTION reject_ledger_mutation()
RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'ledger table % is append-only: % denied',
    TG_TABLE_NAME, TG_OP;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER legal_wrappers_immutable
  BEFORE UPDATE OR DELETE ON legal_wrappers
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

CREATE TRIGGER nav_calculations_immutable
  BEFORE UPDATE OR DELETE ON nav_calculations
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

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
```

### 2. Seed test data (optional)

Run `seed_test.sql` in the PostgreSQL panel to pre-populate a fund, three institutional investors (Citadel, Fidelity, Goldman), and their token positions. Useful for inspecting the full data model before running the Rust service.

### 3. Run the service

CoderPad sets `DATABASE_URL` automatically. Hit **Run** and the demo scenario executes nine steps end-to-end:

```
step 1 → asset registered (BTTF $500M T-bill fund)
step 2 → Delaware Trust legal wrapper created
step 3 → institutional investor onboarded and wallet whitelisted
step 4 → 50,000,000 tokens minted ($50M fiat received)
step 5 → mint confirmed on-chain (tx_hash + block_number recorded)
step 6 → daily yield distributed (5% APY → ~$68,493 daily on $500M)
step 7 → reconciliation passed (DB = on-chain = custodian)
step 8 → 10,000,000 token redemption requested
step 9 → outbox flushed to Kafka (all events delivered)
```

---

## Running Locally

```bash
# Start a local Postgres instance
docker run --rm \
  -e POSTGRES_PASSWORD=password \
  -e POSTGRES_DB=rustrwa \
  -p 5432:5432 \
  postgres:16

# Apply the schema
psql postgres://postgres:password@localhost/rustrwa -f schema.sql

# Start a local Kafka instance (required for outbox publishing)
docker run --rm \
  -e KAFKA_NODE_ID=1 \
  -e KAFKA_PROCESS_ROLES=broker,controller \
  -e KAFKA_LISTENERS=PLAINTEXT://0.0.0.0:9092,CONTROLLER://0.0.0.0:9093 \
  -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://localhost:9092 \
  -e KAFKA_CONTROLLER_QUORUM_VOTERS=1@localhost:9093 \
  -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
  -e CLUSTER_ID=MkU3OEVBNTcwNTJENDM2Qk \
  -p 9092:9092 \
  apache/kafka:3.7.0

# Run the service
DATABASE_URL=postgres://postgres:password@localhost/rustrwa \
KAFKA_BROKERS=localhost:9092 \
cargo run
```

**Environment variables:**

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | `postgres://postgres@localhost/rustrwa` | PostgreSQL connection string |
| `KAFKA_BROKERS` | `localhost:9092` | Kafka bootstrap servers |

---

## Full Asset Lifecycle

```
                    ┌─────────────────────────────────────────────────────┐
                    │              ASSET ONBOARDING                        │
                    │                                                       │
                    │  RwaRegistry      →    LegalWrapper                  │
                    │  (register asset)      (SPV/Trust/Fund)              │
                    │  status: PENDING_LEGAL → PENDING_AUDIT               │
                    └─────────────────────────────────────────────────────┘
                                            │
                                            ▼
                    ┌─────────────────────────────────────────────────────┐
                    │              INVESTOR ONBOARDING                     │
                    │                                                       │
                    │  SanctionsChecker  →  KycProvider  →  Whitelist     │
                    │  (OFAC/EU/UN)         (Securitize)     (wallet)      │
                    └─────────────────────────────────────────────────────┘
                                            │
                                            ▼
                    ┌─────────────────────────────────────────────────────┐
                    │              TOKEN MINTING                           │
                    │                                                       │
                    │  Fiat Wire  →  Lock Supply  →  Mint Record          │
                    │             →  Signing Queue  →  On-chain Confirm    │
                    └─────────────────────────────────────────────────────┘
                                            │
                                            ▼
                    ┌─────────────────────────────────────────────────────┐
                    │              DAILY OPERATIONS                        │
                    │                                                       │
                    │  NAV Calculation  →  Yield Rebase  →  Reconcile     │
                    │  (total × rate/365)   (new tokens)    (DB=chain=CX) │
                    └─────────────────────────────────────────────────────┘
                                            │
                                            ▼
                    ┌─────────────────────────────────────────────────────┐
                    │              TOKEN REDEMPTION                        │
                    │                                                       │
                    │  Request  →  Lock Balance  →  Burn On-chain         │
                    │           →  Settle (derived supply released) →  Wire Fiat │
                    └─────────────────────────────────────────────────────┘
```

---

## Outbox Event Catalogue

Every state change produces a durable outbox event consumed by downstream services:

| Event | Trigger | Downstream Consumer |
|---|---|---|
| `rwa.registered` | Asset registered | Legal review workflow |
| `rwa.legal_wrapper_created` | Wrapper created | Audit workflow |
| `investor.whitelisted` | KYC approved | Token platform — allow transfers |
| `investor.whitelist_revoked` | KYC expired / sanctions hit | Token platform — block transfers |
| `token.mint_requested` | Fiat received | Signing service |
| `token.mint_confirmed` | On-chain confirmation | Investor dashboard |
| `token.redemption_requested` | Investor requests exit | Signing service (burn) |
| `token.redemption_settled` | Burn confirmed | Wire transfer service |
| `nav.yield_calculated` | Daily NAV run | On-chain rebase minter |

---

## Dependencies

| Crate | Purpose |
|---|---|
| `sqlx` | Async Postgres driver with runtime query binding |
| `tokio` | Async runtime |
| `uuid` | UUID v4 generation |
| `rust_decimal` | Precise decimal arithmetic — no floating point in financial logic |
| `serde` / `serde_json` | Serialization for outbox payloads and signing messages |
| `chrono` | Timestamp types and KYC expiry date arithmetic |
| `thiserror` | Typed error derivation |
| `async-trait` | Async methods in trait definitions |
| `tracing` / `tracing-subscriber` | Structured, levelled logging with `#[instrument]` |
| `rdkafka` | Kafka producer (librdkafka binding) for outbox event delivery |

---

## Production Considerations

This demo omits a significant number of concerns that a real RWA platform would require:

**Compliance and Legal**
- No real KYC/AML provider integration (Securitize, Jumio, Onfido)
- No real sanctions screening (Chainalysis, Elliptic, OFAC API)
- No jurisdiction-specific investor eligibility rules (e.g. Reg D, Reg S)
- No KYC expiry monitoring or automated re-verification workflows
- No legal opinion or regulatory approval tracking

**Blockchain**
- No real ERC-1400 / ERC-3643 security token contract integration
- No real signing service or HSM integration
- No actual on-chain confirmation tracking (ConfirmationTracker not implemented)
- No gas estimation, nonce management, or transaction retry logic
- No rebase minter implementation for daily yield distribution

**Security**
- No authentication or authorisation on any endpoint
- No secrets management (`DATABASE_URL` passed as plain environment variable)
- No rate limiting, velocity checks, or fraud detection
- No audit log for compliance reporting

**Operations**
- No database migrations (`schema.sql` applied manually — use `sqlx migrate` in production)
- No metrics, distributed tracing, or alerting (beyond the stub `AlertService`)
- No outbox publisher retry backoff or dead-letter queue
- No horizontal scaling considerations for the outbox publisher beyond `SKIP LOCKED`

**Data Integrity**
- No soft deletes — whitelist revocation uses `DELETE`
- No event versioning or schema evolution on outbox payloads
- No idempotency key expiry or cleanup
- Stub reconciliation services mirror the DB rather than calling real external APIs

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).

---

*Built with ♥️ by [Pavon Dunbar](https://linktr.ee/pavondunbar)*
