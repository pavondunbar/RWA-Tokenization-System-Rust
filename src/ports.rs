use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    models::{BurnSigningMessage, InvestorTier, MintSigningMessage},
};

// -----------------------------------------------------------------------
// Custodian Registry
// Verifies that a named custodian is licensed and on the approved list.
// Real implementation: database lookup of approved custodian registry.
// -----------------------------------------------------------------------
#[async_trait]
pub trait CustodianRegistry: Send + Sync {
    async fn is_approved(&self, custodian: &str) -> Result<bool, RwaError>;
}

// -----------------------------------------------------------------------
// Sanctions Checker
// Screens investors against OFAC, EU, and UN sanctions lists.
// Called OUTSIDE DB transactions — never hold a row lock during this.
// -----------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct SanctionsResult {
    pub is_sanctioned: bool,
    pub list_name:     Option<String>,
}

#[async_trait]
pub trait SanctionsChecker: Send + Sync {
    async fn screen(
        &self,
        investor_id: Uuid,
        jurisdiction: &str,
    ) -> Result<SanctionsResult, RwaError>;
}

// -----------------------------------------------------------------------
// KYC Provider
// Verifies investor identity and accreditation tier (Securitize, etc.).
// Called OUTSIDE DB transactions — never hold a row lock during this.
// -----------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct KycResult {
    pub passed:       bool,
    pub reason:       Option<String>,
    pub reference_id: String,
    pub expiry_date:  DateTime<Utc>,
}

#[async_trait]
pub trait KycProvider: Send + Sync {
    async fn verify(
        &self,
        investor_id: Uuid,
        tier: &InvestorTier,
    ) -> Result<KycResult, RwaError>;
}

// -----------------------------------------------------------------------
// Signing Queue
// Publishes mint/burn transactions to the HSM signing queue (SQS FIFO).
// asset_id as group ID guarantees per-asset FIFO ordering.
// -----------------------------------------------------------------------
#[async_trait]
pub trait MintSigningQueue: Send + Sync {
    // group_id      → SQS MessageGroupId     (FIFO ordering per asset)
    // dedup_id      → SQS MessageDeduplicationId (idempotent enqueue)
    async fn send_mint(
        &self,
        message: MintSigningMessage,
        group_id: String,
        dedup_id: String,
    ) -> Result<(), RwaError>;

    async fn send_burn(
        &self,
        message: BurnSigningMessage,
        group_id: String,
        dedup_id: String,
    ) -> Result<(), RwaError>;
}

// -----------------------------------------------------------------------
// Oracle Service
// Provides external price feeds for NAV calculation.
// -----------------------------------------------------------------------
#[async_trait]
pub trait OracleService: Send + Sync {
    async fn get_yield_rate(&self, asset_id: Uuid) -> Result<rust_decimal::Decimal, RwaError>;
}

// -----------------------------------------------------------------------
// Blockchain Service
// Reads on-chain state for reconciliation.
// -----------------------------------------------------------------------
#[async_trait]
pub trait BlockchainService: Send + Sync {
    async fn get_total_supply(&self, asset_id: Uuid) -> Result<rust_decimal::Decimal, RwaError>;
}

// -----------------------------------------------------------------------
// Custodian API
// Fetches NAV directly from the custodian (BNY Mellon, State Street, etc.).
// -----------------------------------------------------------------------
#[async_trait]
pub trait CustodianApi: Send + Sync {
    async fn get_nav(&self, asset_id: Uuid) -> Result<rust_decimal::Decimal, RwaError>;
}

// -----------------------------------------------------------------------
// Alert Service
// Fires critical alerts when reconciliation or invariant checks fail.
// -----------------------------------------------------------------------
#[async_trait]
pub trait AlertService: Send + Sync {
    async fn critical(
        &self,
        message: &str,
        details: serde_json::Value,
    ) -> Result<(), RwaError>;
}

// -----------------------------------------------------------------------
// Kafka Producer
// Used by the outbox publisher to deliver events.
// -----------------------------------------------------------------------
#[async_trait]
pub trait KafkaProducer: Send + Sync {
    async fn send(
        &self,
        topic: &str,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), RwaError>;
}
