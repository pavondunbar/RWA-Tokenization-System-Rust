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
use sqlx::Row;
use uuid::Uuid;

use errors::RwaError;
use models::*;
use ports::*;

// -----------------------------------------------------------------------
// Stub adapters — replace with real implementations in production
// -----------------------------------------------------------------------

struct StubCustodianRegistry;

#[async_trait]
impl CustodianRegistry for StubCustodianRegistry {
    async fn is_approved(
        &self,
        custodian: &str,
    ) -> Result<bool, RwaError> {
        tracing::info!(
            %custodian,
            "custodian registry: approved (stub)"
        );
        Ok(!custodian.is_empty())
    }
}

struct StubSanctionsChecker;

#[async_trait]
impl SanctionsChecker for StubSanctionsChecker {
    async fn screen(
        &self,
        investor_id: Uuid,
        jurisdiction: &str,
    ) -> Result<SanctionsResult, RwaError> {
        tracing::info!(
            %investor_id, %jurisdiction,
            "sanctions check: clear (stub)"
        );
        Ok(SanctionsResult {
            is_sanctioned: false,
            list_name: None,
        })
    }
}

struct StubKycProvider;

#[async_trait]
impl KycProvider for StubKycProvider {
    async fn verify(
        &self,
        investor_id: Uuid,
        tier: &InvestorTier,
    ) -> Result<KycResult, RwaError> {
        tracing::info!(
            %investor_id,
            tier = %tier.as_str(),
            "KYC verification: passed (stub)"
        );
        Ok(KycResult {
            passed:       true,
            reason:       None,
            reference_id: format!("KYC-STUB-{investor_id}"),
            expiry_date:  Utc::now() + chrono::Duration::days(365),
        })
    }
}

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
            %group_id, %dedup_id,
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
            %group_id, %dedup_id,
            "signing queue: burn message enqueued (stub)"
        );
        Ok(())
    }
}

struct StubOracleService;

#[async_trait]
impl OracleService for StubOracleService {
    async fn get_yield_rate(
        &self,
        asset_id: Uuid,
    ) -> Result<Decimal, RwaError> {
        tracing::info!(%asset_id, "oracle: yield rate 5% (stub)");
        Ok(Decimal::new(5, 2))
    }
}

struct StubBlockchainService {
    db: sqlx::PgPool,
}

#[async_trait]
impl BlockchainService for StubBlockchainService {
    async fn get_total_supply(
        &self,
        asset_id: Uuid,
    ) -> Result<Decimal, RwaError> {
        let minted_row = sqlx::query(
            r#"
            SELECT COALESCE(SUM(token_amount), 0) AS total
            FROM token_mints WHERE asset_id = $1
            "#,
        )
        .bind(asset_id)
        .fetch_one(&self.db)
        .await?;
        let total_minted: Decimal = minted_row.get("total");

        // Settled redemptions derived from event table
        let redeemed_row = sqlx::query(
            r#"
            SELECT COALESCE(SUM(tr.token_amount), 0) AS total
            FROM token_redemptions tr
            WHERE tr.asset_id = $1
              AND EXISTS (
                SELECT 1 FROM token_redemption_events tre
                WHERE tre.redemption_id = tr.id
                  AND tre.status = 'settled'
              )
            "#,
        )
        .bind(asset_id)
        .fetch_one(&self.db)
        .await?;
        let total_redeemed: Decimal = redeemed_row.get("total");

        let supply = total_minted - total_redeemed;
        tracing::info!(
            %asset_id, %supply,
            "blockchain: total supply (stub, mirrors DB)"
        );
        Ok(supply)
    }
}

/// Stub: derives total_value from base + accrued yield.
struct StubCustodianApi {
    db: sqlx::PgPool,
}

#[async_trait]
impl CustodianApi for StubCustodianApi {
    async fn get_nav(
        &self,
        asset_id: Uuid,
    ) -> Result<Decimal, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT
                a.total_value + COALESCE(
                    (SELECT SUM(n.daily_yield)
                     FROM nav_calculations n
                     WHERE n.asset_id = a.id),
                    0
                ) AS derived_total_value
            FROM rwa_assets a
            WHERE a.id = $1
            "#,
        )
        .bind(asset_id)
        .fetch_optional(&self.db)
        .await?;
        let nav = row
            .map(|r| r.get::<Decimal, _>("derived_total_value"))
            .unwrap_or(Decimal::ZERO);
        tracing::info!(
            %asset_id, %nav,
            "custodian API: NAV (stub, mirrors DB)"
        );
        Ok(nav)
    }
}

struct StubAlertService;

#[async_trait]
impl AlertService for StubAlertService {
    async fn critical(
        &self,
        message: &str,
        details: serde_json::Value,
    ) -> Result<(), RwaError> {
        tracing::error!(
            %message, %details, "CRITICAL ALERT (stub)"
        );
        Ok(())
    }
}

struct RdKafkaProducer {
    producer: rdkafka::producer::FutureProducer,
}

impl RdKafkaProducer {
    fn new(brokers: &str) -> Result<Self, RwaError> {
        let producer = rdkafka::ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .map_err(|e| {
                RwaError::Kafka(format!(
                    "producer creation failed: {e}"
                ))
            })?;
        Ok(Self { producer })
    }
}

#[async_trait]
impl KafkaProducer for RdKafkaProducer {
    async fn send(
        &self,
        topic: &str,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), RwaError> {
        self.producer
            .send(
                rdkafka::producer::FutureRecord::to(topic)
                    .key(key)
                    .payload(value),
                std::time::Duration::from_secs(5),
            )
            .await
            .map_err(|(e, _)| {
                RwaError::Kafka(format!(
                    "send to {topic} failed: {e}"
                ))
            })?;
        tracing::info!(%topic, "kafka: message delivered");
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
        .unwrap_or_else(|_| {
            "postgres://postgres@localhost/rustrwa".into()
        });

    let pool = sqlx::PgPool::connect(&database_url).await?;

    // Build services
    let compliance = Arc::new(
        compliance::KycComplianceService::new(
            pool.clone(),
            Box::new(StubKycProvider),
            Box::new(StubSanctionsChecker),
        ),
    );

    let registry_svc = registry::RwaRegistryService::new(
        pool.clone(),
        Box::new(StubCustodianRegistry),
    );

    let legal_svc = legal::LegalWrapperService::new(
        pool.clone(),
    );

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

    let reconciliation_svc =
        reconciliation::RwaReconciliationEngine::new(
            pool.clone(),
            Box::new(StubBlockchainService {
                db: pool.clone(),
            }),
            Box::new(StubCustodianApi {
                db: pool.clone(),
            }),
            Box::new(StubAlertService),
        );

    let kafka_brokers = std::env::var("KAFKA_BROKERS")
        .unwrap_or_else(|_| "localhost:9092".into());
    let kafka_producer = RdKafkaProducer::new(&kafka_brokers)?;

    let outbox_publisher = outbox::RwaOutboxPublisher::new(
        pool.clone(),
        Box::new(kafka_producer),
        100,
    );

    // -------------------------------------------------------------------
    // Demo scenario: Blackstone Treasury Token Fund (BTTF)
    // -------------------------------------------------------------------

    // 1. Register the real-world asset
    let asset = registry_svc
        .register_asset(RegisterAssetRequest {
            asset_type:      AssetType::TreasuryBond,
            name:            "Blackstone Treasury Token Fund (BTTF)"
                .into(),
            total_value:     Decimal::new(500_000_000, 0),
            jurisdiction:    "Delaware, United States".into(),
            custodian:       "BNY Mellon Digital Assets".into(),
            idempotency_key: Uuid::new_v4().to_string(),
        })
        .await?;
    tracing::info!(
        asset_id = %asset.id,
        "step 1 complete: asset registered"
    );

    // 2. Create the Delaware Trust legal wrapper
    let wrapper_id = legal_svc
        .create_legal_wrapper(CreateLegalWrapperRequest {
            asset_id:       asset.id,
            structure_type: LegalStructure::Trust,
            jurisdiction:   "Delaware, United States".into(),
            token_supply:   Decimal::new(500_000_000, 0),
        })
        .await?;
    tracing::info!(
        %wrapper_id,
        "step 2 complete: legal wrapper created"
    );

    // 3. Onboard an institutional investor (Citadel Securities)
    let investor_id = Uuid::new_v4();
    let citadel_wallet =
        "0xCitadel1A2B3C4D5E6F7890abcdef123456";

    let compliance_id = compliance
        .onboard_investor(OnboardInvestorRequest {
            investor_id,
            wallet_address:  citadel_wallet.into(),
            jurisdiction:    "Illinois, United States".into(),
            investor_tier:   InvestorTier::Institutional,
            idempotency_key: Uuid::new_v4().to_string(),
        })
        .await?;
    tracing::info!(
        %compliance_id,
        "step 3 complete: investor onboarded"
    );

    // 3b. Whitelist the issuer wallet so minting transfers pass
    sqlx::query(
        r#"
        INSERT INTO wallet_whitelist_events (
            id, investor_id, wallet_address, tier, action
        )
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::nil())
    .bind("ISSUER_WALLET")
    .bind("system")
    .bind(WhitelistAction::Grant.as_str())
    .execute(&pool)
    .await?;

    // 4. Mint 50,000,000 tokens ($50M fiat wired)
    let mint = minting_svc
        .mint_tokens(MintTokensRequest {
            asset_id:        asset.id,
            investor_id,
            wallet_address:  citadel_wallet.into(),
            token_amount:    Decimal::new(50_000_000, 0),
            fiat_received:   Decimal::new(50_000_000, 0),
            idempotency_key: Uuid::new_v4().to_string(),
        })
        .await?;
    tracing::info!(
        mint_id = %mint.id,
        "step 4 complete: tokens minted"
    );

    // 5. Simulate on-chain confirmation
    minting_svc
        .confirm_mint(
            mint.id,
            "0xabcdef1234567890abcdef1234567890\
             abcdef1234567890abcdef1234567890",
            20_000_000,
        )
        .await?;
    tracing::info!("step 5 complete: mint confirmed on-chain");

    // 6. Calculate and distribute daily yield (5% APY)
    let today = chrono::Utc::now()
        .format("%Y-%m-%d")
        .to_string();
    let nav_id = nav_svc
        .calculate_and_distribute_yield(
            asset.id,
            Decimal::new(5, 2),
            &format!("{}:{}", asset.id, today),
        )
        .await?;
    tracing::info!(
        %nav_id, "step 6 complete: daily yield distributed"
    );

    // 7. Reconcile ledger vs on-chain vs custodian
    let reconciled = reconciliation_svc
        .reconcile_asset(asset.id)
        .await?;
    tracing::info!(
        %reconciled,
        "step 7 complete: reconciliation passed"
    );

    // 8. Investor requests redemption of 10M tokens
    let redemption_id = redemption_svc
        .request_redemption(RedemptionRequest {
            asset_id:        asset.id,
            investor_id,
            wallet_address:  citadel_wallet.into(),
            token_amount:    Decimal::new(10_000_000, 0),
            bank_account:    "CITADEL-WIRE-ACCOUNT-001".into(),
            idempotency_key: Uuid::new_v4().to_string(),
        })
        .await?;
    tracing::info!(
        %redemption_id,
        "step 8 complete: redemption requested"
    );

    // 9. Flush all pending outbox events to Kafka
    let published = outbox_publisher
        .poll_and_publish()
        .await?;
    tracing::info!(
        %published,
        "step 9 complete: outbox flushed to Kafka"
    );

    Ok(())
}
