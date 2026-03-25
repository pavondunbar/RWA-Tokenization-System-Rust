use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    models::{AssetStatus, CreateLegalWrapperRequest},
};

pub struct LegalWrapperService {
    db: PgPool,
}

impl LegalWrapperService {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    /// Creates the legal entity that legally owns the real-world asset.
    /// Also advances the asset state machine from
    /// PENDING_LEGAL -> PENDING_AUDIT via an append-only status event.
    #[instrument(skip(self), fields(
        asset_id     = %req.asset_id,
        structure    = %req.structure_type.as_str(),
        token_supply = %req.token_supply,
    ))]
    pub async fn create_legal_wrapper(
        &self,
        req: CreateLegalWrapperRequest,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Read asset base data (immutable row)
        let row = sqlx::query(
            r#"
            SELECT id, total_value
            FROM rwa_assets
            WHERE id = $1
            "#,
        )
        .bind(req.asset_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::AssetNotFound {
            asset_id: req.asset_id,
        })?;

        let total_value: Decimal = row.get("total_value");

        // Read latest status from event log
        let status_row = sqlx::query(
            r#"
            SELECT status
            FROM rwa_asset_status_events
            WHERE asset_id = $1
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(req.asset_id)
        .fetch_optional(&mut *db_tx)
        .await?;

        let status = status_row
            .map(|r| r.get::<String, _>("status"))
            .unwrap_or_default();

        if status != AssetStatus::PendingLegal.as_str() {
            return Err(RwaError::InvalidState {
                expected: AssetStatus::PendingLegal.as_str().into(),
                got:      status,
            });
        }

        let price_per_token = total_value / req.token_supply;

        let wrapper_id = Uuid::new_v4();

        sqlx::query(
            r#"
            INSERT INTO legal_wrappers (
                id, asset_id, structure_type, jurisdiction,
                token_supply, price_per_token, status, created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, 'active', NOW())
            "#,
        )
        .bind(wrapper_id)
        .bind(req.asset_id)
        .bind(req.structure_type.as_str())
        .bind(&req.jurisdiction)
        .bind(req.token_supply)
        .bind(price_per_token)
        .execute(&mut *db_tx)
        .await?;

        // Append status transition event instead of UPDATE
        sqlx::query(
            r#"
            INSERT INTO rwa_asset_status_events (id, asset_id, status)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(req.asset_id)
        .bind(AssetStatus::PendingAudit.as_str())
        .execute(&mut *db_tx)
        .await?;

        let payload = json!({
            "asset_id":        req.asset_id.to_string(),
            "wrapper_id":      wrapper_id.to_string(),
            "structure":       req.structure_type.as_str(),
            "token_supply":    req.token_supply.to_string(),
            "price_per_token": price_per_token.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'rwa.legal_wrapper_created', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(req.asset_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(
            asset_id        = %req.asset_id,
            %wrapper_id,
            price_per_token = %price_per_token,
            "legal wrapper created — asset advancing to pending_audit"
        );

        Ok(wrapper_id)
    }
}
