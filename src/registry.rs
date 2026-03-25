use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    models::{AssetStatus, RegisterAssetRequest, RwaAsset},
};

pub struct RwaRegistryService {
    db:                  PgPool,
    custodian_registry:  Box<dyn crate::ports::CustodianRegistry>,
}

impl RwaRegistryService {
    pub fn new(
        db: PgPool,
        custodian_registry: Box<dyn crate::ports::CustodianRegistry>,
    ) -> Self {
        Self { db, custodian_registry }
    }

    #[instrument(skip(self), fields(
        name       = %req.name,
        asset_type = %req.asset_type.as_str(),
        custodian  = %req.custodian,
    ))]
    pub async fn register_asset(
        &self,
        req: RegisterAssetRequest,
    ) -> Result<RwaAsset, RwaError> {

        if let Some(existing) = self.find_by_idempotency_key(
            &req.idempotency_key,
        ).await? {
            info!(
                idempotency_key = %req.idempotency_key,
                asset_id        = %existing.id,
                "duplicate registration — returning existing asset"
            );
            return Ok(existing);
        }

        let approved = self.custodian_registry
            .is_approved(&req.custodian)
            .await?;
        if !approved {
            return Err(RwaError::InvalidCustodian {
                custodian: req.custodian.clone(),
            });
        }

        let asset = self.insert_asset_and_outbox(&req).await?;

        info!(
            asset_id   = %asset.id,
            total_value = %req.total_value,
            "asset registered — pending legal review"
        );

        Ok(asset)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<RwaAsset>, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT id, type, name, total_value, jurisdiction,
                   custodian, status, idempotency_key, created_at
            FROM rwa_assets
            WHERE idempotency_key = $1
            "#,
        )
        .bind(key)
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| map_asset_row(&r)))
    }

    async fn insert_asset_and_outbox(
        &self,
        req: &RegisterAssetRequest,
    ) -> Result<RwaAsset, RwaError> {
        let mut db_tx = self.db.begin().await?;

        let asset_id = Uuid::new_v4();

        let row = sqlx::query(
            r#"
            INSERT INTO rwa_assets (
                id, type, name, total_value, jurisdiction,
                custodian, status, idempotency_key, created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
            RETURNING
                id, type, name, total_value, jurisdiction,
                custodian, status, idempotency_key, created_at
            "#,
        )
        .bind(asset_id)
        .bind(req.asset_type.as_str())
        .bind(&req.name)
        .bind(req.total_value)
        .bind(&req.jurisdiction)
        .bind(&req.custodian)
        .bind(AssetStatus::PendingLegal.as_str())
        .bind(&req.idempotency_key)
        .fetch_one(&mut *db_tx)
        .await?;

        let asset = map_asset_row(&row);

        // Record initial status event
        sqlx::query(
            r#"
            INSERT INTO rwa_asset_status_events (id, asset_id, status)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(asset_id)
        .bind(AssetStatus::PendingLegal.as_str())
        .execute(&mut *db_tx)
        .await?;

        let payload = json!({
            "asset_id":     asset_id.to_string(),
            "type":         req.asset_type.as_str(),
            "name":         req.name,
            "total_value":  req.total_value.to_string(),
            "jurisdiction": req.jurisdiction,
            "custodian":    req.custodian,
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'rwa.registered', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(asset_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        Ok(asset)
    }
}

// -----------------------------------------------------------------------
// Row mapping helper
// -----------------------------------------------------------------------

pub fn map_asset_row(row: &sqlx::postgres::PgRow) -> RwaAsset {
    RwaAsset {
        id:              row.get("id"),
        asset_type:      row.get("type"),
        name:            row.get("name"),
        total_value:     row.get("total_value"),
        jurisdiction:    row.get("jurisdiction"),
        custodian:       row.get("custodian"),
        status:          row.get("status"),
        idempotency_key: row.get("idempotency_key"),
        created_at:      row.get("created_at"),
    }
}
