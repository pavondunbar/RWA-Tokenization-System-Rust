use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// -----------------------------------------------------------------------
// Enums — state machines
// -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetType {
    RealEstate,
    TreasuryBond,
    PrivateEquity,
    Commodity,
    Fund,
}

impl AssetType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RealEstate    => "real_estate",
            Self::TreasuryBond  => "treasury_bond",
            Self::PrivateEquity => "private_equity",
            Self::Commodity     => "commodity",
            Self::Fund          => "fund",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetStatus {
    PendingLegal,
    PendingAudit,
    Approved,
    Tokenized,
    Redeemed,
}

impl AssetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PendingLegal  => "pending_legal",
            Self::PendingAudit  => "pending_audit",
            Self::Approved      => "approved",
            Self::Tokenized     => "tokenized",
            Self::Redeemed      => "redeemed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LegalStructure {
    Spv,    // Special Purpose Vehicle — real estate
    Trust,  // Delaware Trust — bonds, funds
    Llc,    // LLC — private equity
    Fund,   // Regulated fund — BlackRock BUIDL
}

impl LegalStructure {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Spv   => "SPV",
            Self::Trust => "TRUST",
            Self::Llc   => "LLC",
            Self::Fund  => "FUND",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvestorTier {
    Retail,        // Limited access
    Accredited,    // $1M+ net worth
    Qualified,     // $5M+ investments
    Institutional, // Banks, funds, hedge funds
}

impl InvestorTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Retail        => "retail",
            Self::Accredited    => "accredited",
            Self::Qualified     => "qualified",
            Self::Institutional => "institutional",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl ComplianceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending  => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired  => "expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MintStatus {
    Pending,
    Approved,
    Signed,
    Broadcast,
    Confirmed,
    Failed,
}

impl MintStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending   => "pending",
            Self::Approved  => "approved",
            Self::Signed    => "signed",
            Self::Broadcast => "broadcast",
            Self::Confirmed => "confirmed",
            Self::Failed    => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedemptionStatus {
    Pending,
    Approved,
    Burning, // Tokens being burned on-chain
    Burned,  // Tokens confirmed burned
    Settled, // Fiat wired back to investor
    Failed,
}

impl RedemptionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending  => "pending",
            Self::Approved => "approved",
            Self::Burning  => "burning",
            Self::Burned   => "burned",
            Self::Settled  => "settled",
            Self::Failed   => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhitelistAction {
    Grant,
    Revoke,
}

impl WhitelistAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Grant  => "grant",
            Self::Revoke => "revoke",
        }
    }
}

// -----------------------------------------------------------------------
// Database row types
// -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RwaAsset {
    pub id:              Uuid,
    pub asset_type:      String,
    pub name:            String,
    pub total_value:     Decimal,
    pub jurisdiction:    String,
    pub custodian:       String,
    pub status:          String,
    pub idempotency_key: Option<String>,
    pub created_at:      DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct RwaAssetStatusEvent {
    pub id:         Uuid,
    pub asset_id:   Uuid,
    pub status:     String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LegalWrapper {
    pub id:              Uuid,
    pub asset_id:        Uuid,
    pub structure_type:  String,
    pub jurisdiction:    String,
    pub token_supply:    Decimal,
    pub price_per_token: Decimal,
    pub status:          String,
    pub created_at:      DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct InvestorCompliance {
    pub id:              Uuid,
    pub investor_id:     Uuid,
    pub wallet_address:  String,
    pub tier:            String,
    pub jurisdiction:    String,
    pub status:          String,
    pub kyc_reference:   Option<String>,
    pub idempotency_key: Option<String>,
    pub created_at:      DateTime<Utc>,
    pub expires_at:      DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ComplianceStatusEvent {
    pub id:            Uuid,
    pub compliance_id: Uuid,
    pub investor_id:   Uuid,
    pub status:        String,
    pub reason:        Option<String>,
    pub created_at:    DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct WalletWhitelistEvent {
    pub id:             Uuid,
    pub investor_id:    Uuid,
    pub wallet_address: String,
    pub tier:           String,
    pub action:         String,
    pub reason:         Option<String>,
    pub created_at:     DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct TokenMint {
    pub id:              Uuid,
    pub asset_id:        Uuid,
    pub investor_id:     Uuid,
    pub wallet_address:  String,
    pub token_amount:    Decimal,
    pub fiat_received:   Decimal,
    pub status:          String,
    pub idempotency_key: Option<String>,
    pub tx_hash:         Option<String>,
    pub block_number:    Option<i64>,
    pub created_at:      DateTime<Utc>,
    pub confirmed_at:    Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct TokenMintEvent {
    pub id:           Uuid,
    pub mint_id:      Uuid,
    pub status:       String,
    pub tx_hash:      Option<String>,
    pub block_number: Option<i64>,
    pub created_at:   DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct TokenRedemption {
    pub id:              Uuid,
    pub asset_id:        Uuid,
    pub investor_id:     Uuid,
    pub wallet_address:  String,
    pub token_amount:    Decimal,
    pub bank_account:    String,
    pub status:          String,
    pub idempotency_key: Option<String>,
    pub created_at:      DateTime<Utc>,
    pub settled_at:      Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct TokenRedemptionEvent {
    pub id:            Uuid,
    pub redemption_id: Uuid,
    pub status:        String,
    pub created_at:    DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NavCalculation {
    pub id:              Uuid,
    pub asset_id:        Uuid,
    pub total_value:     Decimal,
    pub daily_yield:     Decimal,
    pub yield_rate:      Decimal,
    pub calculated_at:   DateTime<Utc>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutboxPublishRecord {
    pub id:           Uuid,
    pub event_id:     Uuid,
    pub published_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub id:           Uuid,
    pub entry_date:   DateTime<Utc>,
    pub event_type:   String,
    pub reference_id: Uuid,
    pub account:      String,
    pub asset_id:     Uuid,
    pub debit:        Decimal,
    pub credit:       Decimal,
    pub currency:     String,
    pub memo:         Option<String>,
    pub created_at:   DateTime<Utc>,
}

// -----------------------------------------------------------------------
// Ledger account naming helpers
// -----------------------------------------------------------------------

pub struct LedgerAccount;

impl LedgerAccount {
    pub fn fund_treasury(asset_id: Uuid) -> String {
        format!("fund:treasury:{asset_id}")
    }

    pub fn fund_fiat(asset_id: Uuid) -> String {
        format!("fund:fiat:{asset_id}")
    }

    pub fn fund_yield(asset_id: Uuid) -> String {
        format!("fund:yield:{asset_id}")
    }

    pub fn investor_tokens(investor_id: Uuid) -> String {
        format!("investor:tokens:{investor_id}")
    }

    pub fn investor_fiat(investor_id: Uuid) -> String {
        format!("investor:fiat:{investor_id}")
    }
}

// -----------------------------------------------------------------------
// Request types — service layer I/O
// -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RegisterAssetRequest {
    pub asset_type:      AssetType,
    pub name:            String,
    pub total_value:     Decimal,
    pub jurisdiction:    String,
    pub custodian:       String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone)]
pub struct CreateLegalWrapperRequest {
    pub asset_id:        Uuid,
    pub structure_type:  LegalStructure,
    pub jurisdiction:    String,
    pub token_supply:    Decimal,
}

#[derive(Debug, Clone)]
pub struct OnboardInvestorRequest {
    pub investor_id:     Uuid,
    pub wallet_address:  String,
    pub jurisdiction:    String,
    pub investor_tier:   InvestorTier,
    pub idempotency_key: String,
}

#[derive(Debug, Clone)]
pub struct MintTokensRequest {
    pub asset_id:        Uuid,
    pub investor_id:     Uuid,
    pub wallet_address:  String,
    pub token_amount:    Decimal,
    pub fiat_received:   Decimal,
    pub idempotency_key: String,
}

#[derive(Debug, Clone)]
pub struct RedemptionRequest {
    pub asset_id:        Uuid,
    pub investor_id:     Uuid,
    pub wallet_address:  String,
    pub token_amount:    Decimal,
    pub bank_account:    String,
    pub idempotency_key: String,
}

// -----------------------------------------------------------------------
// Signing queue message types
// -----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintSigningMessage {
    pub mint_id:      String,
    pub asset_id:     String,
    pub wallet:       String,
    pub token_amount: String,
    pub action:       String, // "mint"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnSigningMessage {
    pub redemption_id: String,
    pub asset_id:      String,
    pub wallet:        String,
    pub token_amount:  String,
    pub action:        String, // "burn"
}
