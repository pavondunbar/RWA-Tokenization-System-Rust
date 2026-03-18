use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::errors::RwaError;

/// Scale factor for dividing annual yield rate into a daily rate.
/// daily_yield = total_value * (annual_rate / 365)
const DAYS_PER_YEAR: &str = "365";

pub struct NavCalculationEngine {
    db: PgPool,
}

impl NavCalculationEngine {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    /// Calculates daily yield and distributes it across the fund.
    ///
    /// BlackRock BUIDL rebase mechanism:
    ///   - Fund holds T-bills earning ~5% APY
    ///   - Daily yield = total_value × (annual_rate / 365)
    ///   - Yield distributed by minting new tokens to investors (positive rebase)
    ///   - Token price stays $1.00 — investor's balance increases, not the price
    ///
    /// Example:
    ///   - Investor holds 10,000,000 BUIDL tokens
    ///   - Daily rate = 5% / 365 = 0.01370%
    ///   - Daily yield = 10,000,000 × 0.0001370 = 1,370 new tokens
    ///   - Next day: 10,001,370 tokens, all at $1.00
    ///
    /// Idempotency key should be "asset_id:YYYY-MM-DD" to prevent
    /// double-calculating yield on the same day.
    #[instrument(skip(self), fields(
        asset_id           = %asset_id,
        annual_yield_rate  = %annual_yield_rate,
    ))]
    pub async fn calculate_and_distribute_yield(
        &self,
        asset_id: Uuid,
        annual_yield_rate: Decimal,
        idempotency_key: &str,
    ) -> Result<Uuid, RwaError> {

        // ---- STEP 1: Idempotency check — one NAV calc per asset per day -------
        if let Some(existing_id) = self.find_nav_by_idempotency_key(idempotency_key).await? {
            info!(
                idempotency_key = %idempotency_key,
                nav_id          = %existing_id,
                "duplicate NAV calculation — returning existing record"
            );
            return Ok(existing_id);
        }

        // ---- STEP 2: Lock asset and compute yield ----------------------------
        let nav_id = self.lock_and_compute_yield(asset_id, annual_yield_rate, idempotency_key).await?;

        info!(
            %asset_id,
            %nav_id,
            %annual_yield_rate,
            "NAV yield calculated and distributed"
        );

        Ok(nav_id)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn lock_and_compute_yield(
        &self,
        asset_id: Uuid,
        annual_yield_rate: Decimal,
        idempotency_key: &str,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Pessimistic lock — only one NAV update at a time per asset
        let row = sqlx::query(
            r#"
            SELECT id, total_value
            FROM rwa_assets
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(asset_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::AssetNotFound { asset_id })?;

        let total_value: Decimal = row.get("total_value");

        // Daily yield = total_value × (annual_rate / 365)
        let days = Decimal::from_str_exact(DAYS_PER_YEAR).unwrap();
        let daily_yield = total_value * annual_yield_rate / days;

        let nav_id = Uuid::new_v4();

        sqlx::query(
            r#"
            INSERT INTO nav_calculations (
                id, asset_id, total_value, daily_yield,
                yield_rate, calculated_at, idempotency_key
            )
            VALUES ($1, $2, $3, $4, $5, NOW(), $6)
            "#,
        )
        .bind(nav_id)
        .bind(asset_id)
        .bind(total_value)
        .bind(daily_yield)
        .bind(annual_yield_rate)
        .bind(idempotency_key)
        .execute(&mut *db_tx)
        .await?;

        // Accrue yield — update total fund value
        sqlx::query(
            r#"
            UPDATE rwa_assets
            SET total_value = total_value + $1,
                updated_at  = NOW()
            WHERE id = $2
            "#,
        )
        .bind(daily_yield)
        .bind(asset_id)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — triggers on-chain token rebase
        // The rebase minter reads daily_yield and mints proportionally to each holder
        let payload = json!({
            "asset_id":    asset_id.to_string(),
            "nav_id":      nav_id.to_string(),
            "daily_yield": daily_yield.to_string(),
            "yield_rate":  annual_yield_rate.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'nav.yield_calculated', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(nav_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(
            %asset_id,
            %nav_id,
            %total_value,
            %daily_yield,
            "yield accrued to fund — rebase event written to outbox"
        );

        Ok(nav_id)
    }

    async fn find_nav_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<Uuid>, RwaError> {
        let row = sqlx::query(
            "SELECT id FROM nav_calculations WHERE idempotency_key = $1",
        )
        .bind(key)
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| r.get("id")))
    }
}
