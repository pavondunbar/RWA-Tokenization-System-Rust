use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    models::LedgerAccount,
};

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
    /// Yield is derived from the current total value:
    ///   base total_value (immutable in rwa_assets) +
    ///   SUM(daily_yield) from all prior nav_calculations.
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

        if let Some(existing_id) = self
            .find_nav_by_idempotency_key(idempotency_key)
            .await?
        {
            info!(
                idempotency_key = %idempotency_key,
                nav_id          = %existing_id,
                "duplicate NAV calculation — returning existing record"
            );
            return Ok(existing_id);
        }

        let nav_id = self
            .compute_and_record_yield(
                asset_id,
                annual_yield_rate,
                idempotency_key,
            )
            .await?;

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

    async fn compute_and_record_yield(
        &self,
        asset_id: Uuid,
        annual_yield_rate: Decimal,
        idempotency_key: &str,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Derive current total value: base + all accrued yield
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
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::AssetNotFound { asset_id })?;

        let total_value: Decimal = row.get("derived_total_value");

        let days = Decimal::from_str_exact(DAYS_PER_YEAR)
            .expect("constant is valid");
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

        // Double-entry ledger: yield distribution
        // DEBIT fund:yield (yield generated)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'nav.yield', $2, $3, $4, $5, 0, 'TOKEN',
                    'daily yield distribution')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(nav_id)
        .bind(LedgerAccount::fund_yield(asset_id))
        .bind(asset_id)
        .bind(daily_yield)
        .execute(&mut *db_tx)
        .await?;

        // CREDIT fund:treasury (tokens to distribute)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'nav.yield', $2, $3, $4, 0, $5, 'TOKEN',
                    'daily yield distribution')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(nav_id)
        .bind(LedgerAccount::fund_treasury(asset_id))
        .bind(asset_id)
        .bind(daily_yield)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — triggers on-chain token rebase
        let payload = json!({
            "asset_id":    asset_id.to_string(),
            "nav_id":      nav_id.to_string(),
            "daily_yield": daily_yield.to_string(),
            "yield_rate":  annual_yield_rate.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
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
            "SELECT id FROM nav_calculations \
             WHERE idempotency_key = $1",
        )
        .bind(key)
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| r.get("id")))
    }
}
