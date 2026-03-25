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
        Self {
            db, blockchain, custodian_api, alert_service,
        }
    }

    /// Compares internal ledger state against on-chain reality
    /// and custodian report. Runs daily, after NAV calculation.
    #[instrument(skip(self), fields(%asset_id))]
    pub async fn reconcile_asset(
        &self,
        asset_id: Uuid,
    ) -> Result<bool, RwaError> {

        // Derive total value: base + all accrued yield
        let value_row = sqlx::query(
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
        .await?
        .ok_or(RwaError::AssetNotFound { asset_id })?;

        let total_value: Decimal =
            value_row.get("derived_total_value");

        // Derive allocated supply from append-only ledger tables
        let minted_row = sqlx::query(
            r#"
            SELECT COALESCE(SUM(token_amount), 0) AS total
            FROM token_mints
            WHERE asset_id = $1
            "#,
        )
        .bind(asset_id)
        .fetch_one(&self.db)
        .await?;
        let total_minted: Decimal = minted_row.get("total");

        // Only settled redemptions reduce supply.
        // Status derived from token_redemption_events.
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

        let minted_supply = total_minted - total_redeemed;

        // External service calls — OUTSIDE any DB transaction
        let onchain_supply = self.blockchain
            .get_total_supply(asset_id)
            .await?;
        let custodian_nav = self.custodian_api
            .get_nav(asset_id)
            .await?;

        let mut mismatches: Vec<serde_json::Value> = vec![];
        let tolerance = Decimal::from_str_exact(NAV_TOLERANCE)
            .expect("constant is valid");

        if minted_supply != onchain_supply {
            mismatches.push(json!({
                "type":     "supply_mismatch",
                "internal": minted_supply.to_string(),
                "onchain":  onchain_supply.to_string(),
                "delta":    (minted_supply - onchain_supply)
                    .abs().to_string(),
            }));
        }

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

        if !mismatches.is_empty() {
            let details = json!(mismatches);
            error!(
                %asset_id,
                mismatch_count = mismatches.len(),
                "reconciliation failed — halting operations"
            );
            self.alert_service
                .critical(
                    &format!(
                        "RWA reconciliation failed for asset \
                         {asset_id}"
                    ),
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
            "reconciliation passed — ledger matches on-chain \
             and custodian"
        );

        Ok(true)
    }
}
