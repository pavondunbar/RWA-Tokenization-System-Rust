mod compliance;
mod errors;
mod legal;
mod minting;
mod models;
mod nav;
mod outbox;
mod ports;
mod reconciliation;
mod redemption;
mod registry;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use uuid::Uuid;

use errors::RwaError;
use models::*;
use ports::*;

// -----------------------------------------------------------------------
// Stub adapters — replace with real implementations in production
// -----------------------------------------------------------------------

/// Stub: approves any non-empty custodian name.
/// Real implementation: database lookup of licensed custodian registry.
struct StubCustodianRegistry;

#[async_trait]
impl CustodianRegistry for StubCustodianRegistry {
    async fn is_approved(&self, custodian: &str) -> Result<bool, RwaError> {
        tracing::info!(%custodian, "custodian registry: approved (stub)");
        Ok(!custodian.is_empty())
    }
}

/// Stub: no investor is ever sanctioned.
/// Real implementation: OFAC/EU/UN list API (Chainalysis, Elliptic, etc.).
struct StubSanctionsChecker;

#[async_trait]
impl SanctionsChecker for StubSanctionsChecker {
    async fn screen(&self, investor_id: Uuid, jurisdiction: &str) -> Result<SanctionsResult, RwaError> {
        tracing::info!(%investor_id, %jurisdiction, "sanctions check: clear (stub)");
        Ok(SanctionsResult { is_sanctioned: false, list_name: None })
    }
}

/// Stub: all investors pass KYC. Expires in 1 year.
/// Real implementation: Securitize, Jumio, or Onfido KYC/AML API.
struct StubKycProvider;

#[async_trait]
impl KycProvider for StubKycProvider {
    async fn verify(&self, investor_id: Uuid, tier: &InvestorTier) -> Result<KycResult, RwaError> {
        tracing::info!(%investor_id, tier = %tier.as_str(), "KYC verification: passed (stub)");
        Ok(KycResult {
            passed:       true,
            reason:       None,
            reference_id: format!("KYC-STUB-{}", investor_id),
            expiry_date:  Utc::now() + chrono::Duration::days(365),
        })
    }
}

/// Stub: logs and discards mint/burn messages.
/// Real implementation: AWS SQS FIFO queue with HSM signing worker.
struct StubSigningQueue;

#[async_trait]
impl MintSigningQueue for StubSigningQueue {
    async fn send_mint(
        &self,
        message: MintSigningMessage,
        group_id: String,
        dedup_id: String,
    ) -> Result<(), RwaError> {
        tracing::info!(
            mint_id  = %message.mint_id,
            %group_id,
            %dedup_id,
            "signing queue: mint message enqueued (stub)"
        );
        Ok(())
    }

    async fn send_burn(
        &self,
        message: BurnSigningMessage,
        group_id: String,
        dedup_id: String,
    ) -> Result<(), RwaError> {
        tracing::info!(
            redemption_id = %message.redemption_id,
            %group_id,
            %dedup_id,
            "signing queue: burn message enqueued (stub)"
        );
        Ok(())
    }
}

/// Stub: returns a fixed 5% annual yield rate.
/// Real implementation: on-chain oracle or custodian rate feed.
struct StubOracleService;

#[async_trait]
impl OracleService for StubOracleService {
    async fn get_yield_rate(&self, asset_id: Uuid) -> Result<Decimal, RwaError> {
        tracing::info!(%asset_id, "oracle: yield rate 5% (stub)");
        Ok(Decimal::new(5, 2)) // 0.05 = 5%
    }
}

/// Stub: always returns minted_supply as on-chain supply (perfect match).
/// Real implementation: calls ERC-1400 totalSupply() via Ethereum node.
struct StubBlockchainService {
    db: sqlx::PgPool,
}

#[async_trait]
impl BlockchainService for StubBlockchainService {
    async fn get_total_supply(&self, asset_id: Uuid) -> Result<Decimal, RwaError> {
        let row = sqlx::query("SELECT minted_supply FROM rwa_token_supply WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_optional(&self.db)
            .await?;
        let supply = row
            .map(|r| r.get::<Decimal, _>("minted_supply"))
            .unwrap_or(Decimal::ZERO);
        tracing::info!(%asset_id, %supply, "blockchain: total supply (stub, mirrors DB)");
        Ok(supply)
    }
}

/// Stub: returns DB total_value as custodian NAV (perfect match).
/// Real implementation: BNY Mellon / State Street custodian API.
struct StubCustodianApi {
    db: sqlx::PgPool,
}

#[async_trait]
impl CustodianApi for StubCustodianApi {
    async fn get_nav(&self, asset_id: Uuid) -> Result<Decimal, RwaError> {
        let row = sqlx::query("SELECT total_value FROM rwa_assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&self.db)
            .await?;
        let nav = row
            .map(|r| r.get::<Decimal, _>("total_value"))
            .unwrap_or(Decimal::ZERO);
        tracing::info!(%asset_id, %nav, "custodian API: NAV (stub, mirrors DB)");
        Ok(nav)
    }
}

/// Stub: logs the alert. Real implementation: PagerDuty, OpsGenie, etc.
struct StubAlertService;

#[async_trait]
impl AlertService for StubAlertService {
    async fn critical(&self, message: &str, details: serde_json::Value) -> Result<(), RwaError> {
        tracing::error!(%message, %details, "CRITICAL ALERT (stub)");
        Ok(())
    }
}

/// Stub: logs and discards Kafka messages.
/// Real implementation: rdkafka or kafka-rust producer.
struct StubKafkaProducer;

#[async_trait]
impl KafkaProducer for StubKafkaProducer {
    async fn send(&self, topic: &str, key: &[u8], value: &[u8]) -> Result<(), RwaError> {
        let key_str   = String::from_utf8_lossy(key);
        let value_str = String::from_utf8_lossy(value);
        tracing::info!(%topic, key = %key_str, value = %value_str, "Kafka: message sent (stub)");
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Wiring and demo scenario
// -----------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:password@localhost/rwa".into());

    let pool = sqlx::PgPool::connect(&database_url).await?;

    // Build services
    let compliance = Arc::new(compliance::KycComplianceService::new(
        pool.clone(),
        Box::new(StubKycProvider),
        Box::new(StubSanctionsChecker),
    ));

    let registry_svc = registry::RwaRegistryService::new(
        pool.clone(),
        Box::new(StubCustodianRegistry),
    );

    let legal_svc = legal::LegalWrapperService::new(pool.clone());

    let minting_svc = minting::TokenMintingService::new(
        pool.clone(),
        Arc::clone(&compliance),
        Box::new(StubSigningQueue),
    );

    let redemption_svc = redemption::TokenRedemptionService::new(
        pool.clone(),
        Arc::clone(&compliance),
        Box::new(StubSigningQueue),
    );

    let nav_svc = nav::NavCalculationEngine::new(pool.clone());

    let reconciliation_svc = reconciliation::RwaReconciliationEngine::new(
        pool.clone(),
        Box::new(StubBlockchainService { db: pool.clone() }),
        Box::new(StubCustodianApi { db: pool.clone() }),
        Box::new(StubAlertService),
    );

    let outbox_publisher = outbox::RwaOutboxPublisher::new(
        pool.clone(),
        Box::new(StubKafkaProducer),
        100,
    );

    // -----------------------------------------------------------------------
    // Demo scenario: Blackstone Treasury Token Fund (BTTF)
    // $500M T-bill fund | $1.00 per token | 500M tokens | Institutional only
    // -----------------------------------------------------------------------

    // 1. Register the real-world asset
    let asset = registry_svc.register_asset(RegisterAssetRequest {
        asset_type:      AssetType::TreasuryBond,
        name:            "Blackstone Treasury Token Fund (BTTF)".into(),
        total_value:     Decimal::new(500_000_000, 0),
        jurisdiction:    "Delaware, United States".into(),
        custodian:       "BNY Mellon Digital Assets".into(),
        idempotency_key: Uuid::new_v4().to_string(),
    }).await?;
    tracing::info!(asset_id = %asset.id, "step 1 complete: asset registered");

    // 2. Create the Delaware Trust legal wrapper
    let wrapper_id = legal_svc.create_legal_wrapper(CreateLegalWrapperRequest {
        asset_id:       asset.id,
        structure_type: LegalStructure::Trust,
        jurisdiction:   "Delaware, United States".into(),
        token_supply:   Decimal::new(500_000_000, 0), // 500M tokens at $1.00
    }).await?;
    tracing::info!(%wrapper_id, "step 2 complete: legal wrapper created");

    // 3. Onboard an institutional investor (Citadel Securities)
    let investor_id = Uuid::new_v4();
    let citadel_wallet = "0xCitadel1A2B3C4D5E6F7890abcdef123456";

    let compliance_id = compliance.onboard_investor(OnboardInvestorRequest {
        investor_id,
        wallet_address:  citadel_wallet.into(),
        jurisdiction:    "Illinois, United States".into(),
        investor_tier:   InvestorTier::Institutional,
        idempotency_key: Uuid::new_v4().to_string(),
    }).await?;
    tracing::info!(%compliance_id, "step 3 complete: investor onboarded");

    // 4. Mint 50,000,000 tokens ($50M fiat wired)
    let mint = minting_svc.mint_tokens(MintTokensRequest {
        asset_id:        asset.id,
        investor_id,
        wallet_address:  citadel_wallet.into(),
        token_amount:    Decimal::new(50_000_000, 0),
        fiat_received:   Decimal::new(50_000_000, 0),
        idempotency_key: Uuid::new_v4().to_string(),
    }).await?;
    tracing::info!(mint_id = %mint.id, "step 4 complete: tokens minted");

    // 5. Simulate on-chain confirmation
    minting_svc.confirm_mint(
        mint.id,
        "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        20_000_000,
    ).await?;
    tracing::info!("step 5 complete: mint confirmed on-chain");

    // 6. Calculate and distribute daily yield (5% APY)
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let nav_id = nav_svc.calculate_and_distribute_yield(
        asset.id,
        Decimal::new(5, 2), // 5% annual yield
        &format!("{}:{}", asset.id, today),
    ).await?;
    tracing::info!(%nav_id, "step 6 complete: daily yield distributed");

    // 7. Reconcile ledger vs on-chain vs custodian
    let reconciled = reconciliation_svc.reconcile_asset(asset.id).await?;
    tracing::info!(%reconciled, "step 7 complete: reconciliation passed");

    // 8. Investor requests redemption of 10M tokens
    let redemption_id = redemption_svc.request_redemption(RedemptionRequest {
        asset_id:        asset.id,
        investor_id,
        wallet_address:  citadel_wallet.into(),
        token_amount:    Decimal::new(10_000_000, 0),
        bank_account:    "CITADEL-WIRE-ACCOUNT-001".into(),
        idempotency_key: Uuid::new_v4().to_string(),
    }).await?;
    tracing::info!(%redemption_id, "step 8 complete: redemption requested");

    // 9. Flush all pending outbox events to Kafka
    let published = outbox_publisher.poll_and_publish().await?;
    tracing::info!(%published, "step 9 complete: outbox flushed to Kafka");

    Ok(())
}
