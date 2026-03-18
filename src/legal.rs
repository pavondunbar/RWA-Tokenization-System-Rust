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
    /// This is the bridge between off-chain ownership and on-chain token representation.
    /// Without this layer the token is just a receipt — no enforceable legal claim.
    ///
    /// Also advances the asset state machine from PENDING_LEGAL → PENDING_AUDIT,
    /// all atomically.
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

        // Pessimistic lock — enforces one legal wrapper per asset.
        // Any concurrent attempt blocks here until we commit.
        let row = sqlx::query(
            r#"
            SELECT id, status, total_value
            FROM rwa_assets
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(req.asset_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::AssetNotFound { asset_id: req.asset_id })?;

        let status: String      = row.get("status");
        let total_value: Decimal = row.get("total_value");

        // State machine guard — asset must be in PENDING_LEGAL before wrapping
        if status != AssetStatus::PendingLegal.as_str() {
            return Err(RwaError::InvalidState {
                expected: AssetStatus::PendingLegal.as_str().into(),
                got:      status,
            });
        }

        // Token economics:
        //   price_per_token = total_value / token_supply
        //   BUIDL: $2.9B / 2.9B tokens = $1.00 per token
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

        // Advance asset state machine: PENDING_LEGAL → PENDING_AUDIT
        sqlx::query(
            r#"
            UPDATE rwa_assets
            SET status     = $1,
                updated_at = NOW()
            WHERE id = $2
            "#,
        )
        .bind(AssetStatus::PendingAudit.as_str())
        .bind(req.asset_id)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — triggers the external audit workflow
        let payload = json!({
            "asset_id":        req.asset_id.to_string(),
            "wrapper_id":      wrapper_id.to_string(),
            "structure":       req.structure_type.as_str(),
            "token_supply":    req.token_supply.to_string(),
            "price_per_token": price_per_token.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
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
