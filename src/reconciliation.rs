use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{error, info, instrument};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    ports::{AlertService, BlockchainService, CustodianApi},
};

/// Tolerance for NAV comparison against custodian report.
/// Any delta above $0.01 is flagged as a mismatch.
const NAV_TOLERANCE: &str = "0.01";

pub struct RwaReconciliationEngine {
    db:               PgPool,
    blockchain:       Box<dyn BlockchainService>,
    custodian_api:    Box<dyn CustodianApi>,
    alert_service:    Box<dyn AlertService>,
}

impl RwaReconciliationEngine {
    pub fn new(
        db: PgPool,
        blockchain: Box<dyn BlockchainService>,
        custodian_api: Box<dyn CustodianApi>,
        alert_service: Box<dyn AlertService>,
    ) -> Self {
        Self { db, blockchain, custodian_api, alert_service }
    }

    /// Compares internal ledger state against on-chain reality and custodian report.
    /// Runs daily, after NAV calculation.
    ///
    /// Checks:
    ///   1. Minted supply in DB == total token supply on-chain
    ///   2. Fund total_value in DB ≈ custodian NAV report (within $0.01 tolerance)
    ///
    /// Any mismatch fires a CRITICAL alert and returns false.
    /// Callers should halt new mints and redemptions until resolved.
    #[instrument(skip(self), fields(%asset_id))]
    pub async fn reconcile_asset(&self, asset_id: Uuid) -> Result<bool, RwaError> {

        // ---- STEP 1: Read internal ledger state ------------------------------
        let row = sqlx::query(
            r#"
            SELECT ts.minted_supply, a.total_value
            FROM rwa_token_supply ts
            JOIN rwa_assets a ON a.id = ts.asset_id
            WHERE ts.asset_id = $1
            "#,
        )
        .bind(asset_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(RwaError::SupplyNotFound { asset_id })?;

        let minted_supply: Decimal = row.get("minted_supply");
        let total_value: Decimal   = row.get("total_value");

        // ---- STEP 2: Read on-chain state and custodian report ----------------
        // Both calls are OUTSIDE any DB transaction — external service calls
        let onchain_supply = self.blockchain.get_total_supply(asset_id).await?;
        let custodian_nav  = self.custodian_api.get_nav(asset_id).await?;

        // ---- STEP 3: Compare and collect mismatches -------------------------
        let mut mismatches: Vec<serde_json::Value> = vec![];
        let tolerance = Decimal::from_str_exact(NAV_TOLERANCE).unwrap();

        // Check 1: Token supply matches on-chain
        if minted_supply != onchain_supply {
            mismatches.push(json!({
                "type":     "supply_mismatch",
                "internal": minted_supply.to_string(),
                "onchain":  onchain_supply.to_string(),
                "delta":    (minted_supply - onchain_supply).abs().to_string(),
            }));
        }

        // Check 2: Fund value matches custodian report (within tolerance)
        let nav_delta = (total_value - custodian_nav).abs();
        if nav_delta > tolerance {
            mismatches.push(json!({
                "type":      "nav_mismatch",
                "internal":  total_value.to_string(),
                "custodian": custodian_nav.to_string(),
                "delta":     nav_delta.to_string(),
                "tolerance": NAV_TOLERANCE,
            }));
        }

        // ---- STEP 4: Alert and halt on any mismatch -------------------------
        if !mismatches.is_empty() {
            let details = json!(mismatches);
            error!(
                %asset_id,
                mismatch_count = mismatches.len(),
                "reconciliation failed — halting mints and redemptions"
            );
            self.alert_service
                .critical(
                    &format!("RWA reconciliation failed for asset {asset_id}"),
                    details.clone(),
                )
                .await?;

            return Err(RwaError::ReconciliationMismatch {
                asset_id,
                details: details.to_string(),
            });
        }

        info!(
            %asset_id,
            %minted_supply,
            %total_value,
            "reconciliation passed — ledger matches on-chain and custodian"
        );

        Ok(true)
    }
}
