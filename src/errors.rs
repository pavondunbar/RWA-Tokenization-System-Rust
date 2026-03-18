use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RwaError {
    #[error("custodian '{custodian}' is not licensed or approved")]
    InvalidCustodian { custodian: String },

    #[error("invalid state transition: expected '{expected}', found '{got}'")]
    InvalidState { expected: String, got: String },

    #[error("wallet '{wallet}' is not on the transfer whitelist")]
    WalletNotWhitelisted { wallet: String },

    #[error("insufficient token supply: {available} available, {requested} requested")]
    InsufficientTokenSupply {
        available: rust_decimal::Decimal,
        requested: rust_decimal::Decimal,
    },

    #[error("insufficient token balance: {available} held, {requested} requested")]
    InsufficientTokenBalance {
        available: rust_decimal::Decimal,
        requested: rust_decimal::Decimal,
    },

    #[error("investor {investor_id} is on a sanctions list")]
    SanctionedInvestor { investor_id: Uuid },

    #[error("KYC verification failed: {reason}")]
    KycFailed { reason: String },

    #[error("asset {asset_id} not found")]
    AssetNotFound { asset_id: Uuid },

    #[error("legal wrapper not found for asset {asset_id}")]
    SupplyNotFound { asset_id: Uuid },

    #[error("mint {mint_id} not found")]
    MintNotFound { mint_id: Uuid },

    #[error("redemption {redemption_id} not found")]
    RedemptionNotFound { redemption_id: Uuid },

    #[error("reconciliation mismatch for asset {asset_id}: {details}")]
    ReconciliationMismatch { asset_id: Uuid, details: String },

    #[error("signing queue error: {0}")]
    SigningQueue(String),

    #[error("blockchain error: {0}")]
    Blockchain(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
