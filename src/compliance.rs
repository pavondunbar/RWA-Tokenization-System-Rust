use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{info, instrument, warn};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    models::{
        ComplianceStatus, InvestorCompliance,
        OnboardInvestorRequest, WhitelistAction,
    },
    ports::{KycProvider, SanctionsChecker},
};

pub struct KycComplianceService {
    db:               PgPool,
    kyc_provider:     Box<dyn KycProvider>,
    sanctions_checker: Box<dyn SanctionsChecker>,
}

impl KycComplianceService {
    pub fn new(
        db: PgPool,
        kyc_provider: Box<dyn KycProvider>,
        sanctions_checker: Box<dyn SanctionsChecker>,
    ) -> Self {
        Self { db, kyc_provider, sanctions_checker }
    }

    /// Onboards an investor through full KYC/AML.
    #[instrument(skip(self), fields(
        investor_id    = %req.investor_id,
        wallet_address = %req.wallet_address,
        tier           = %req.investor_tier.as_str(),
    ))]
    pub async fn onboard_investor(
        &self,
        req: OnboardInvestorRequest,
    ) -> Result<Uuid, RwaError> {

        if let Some(existing) = self
            .find_by_idempotency_key(&req.idempotency_key)
            .await?
        {
            info!(
                idempotency_key = %req.idempotency_key,
                compliance_id   = %existing.id,
                "duplicate onboarding — returning existing"
            );
            return Ok(existing.id);
        }

        let sanctions_result = self.sanctions_checker
            .screen(req.investor_id, &req.jurisdiction)
            .await?;

        if sanctions_result.is_sanctioned {
            warn!(
                investor_id = %req.investor_id,
                list        = ?sanctions_result.list_name,
                "investor failed sanctions screening"
            );
            return Err(RwaError::SanctionedInvestor {
                investor_id: req.investor_id,
            });
        }

        let kyc_result = self.kyc_provider
            .verify(req.investor_id, &req.investor_tier)
            .await?;

        if !kyc_result.passed {
            let reason = kyc_result.reason
                .unwrap_or_else(|| "no reason provided".into());
            warn!(
                investor_id = %req.investor_id,
                %reason,
                "investor failed KYC verification"
            );
            return Err(RwaError::KycFailed { reason });
        }

        let compliance_id = self
            .insert_compliance_and_whitelist(
                &req,
                &kyc_result.reference_id,
                kyc_result.expiry_date,
            )
            .await?;

        info!(
            investor_id    = %req.investor_id,
            %compliance_id,
            wallet_address = %req.wallet_address,
            kyc_reference  = %kyc_result.reference_id,
            "investor onboarded and wallet whitelisted"
        );

        Ok(compliance_id)
    }

    /// Returns true only if BOTH wallets are currently whitelisted.
    /// Checks the latest action in wallet_whitelist_events — a wallet
    /// is whitelisted if its most recent event is 'grant'.
    pub async fn can_transfer(
        &self,
        from_wallet: &str,
        to_wallet: &str,
    ) -> Result<bool, RwaError> {
        let sender_ok = self
            .is_wallet_whitelisted(from_wallet)
            .await?;
        let receiver_ok = self
            .is_wallet_whitelisted(to_wallet)
            .await?;

        Ok(sender_ok && receiver_ok)
    }

    /// Revokes an investor's whitelist status via append-only events.
    #[instrument(skip(self), fields(investor_id = %investor_id))]
    pub async fn revoke_whitelist(
        &self,
        investor_id: Uuid,
        reason: &str,
    ) -> Result<(), RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Look up the investor's wallet(s) and compliance record
        let comp_rows = sqlx::query(
            r#"
            SELECT id, wallet_address, tier
            FROM investor_compliance
            WHERE investor_id = $1
            "#,
        )
        .bind(investor_id)
        .fetch_all(&mut *db_tx)
        .await?;

        // Append revoke events for each wallet
        for comp_row in &comp_rows {
            let wallet: String = comp_row.get("wallet_address");
            let tier: String = comp_row.get("tier");
            let compliance_id: Uuid = comp_row.get("id");

            sqlx::query(
                r#"
                INSERT INTO wallet_whitelist_events (
                    id, investor_id, wallet_address,
                    tier, action, reason
                )
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
            )
            .bind(Uuid::new_v4())
            .bind(investor_id)
            .bind(&wallet)
            .bind(&tier)
            .bind(WhitelistAction::Revoke.as_str())
            .bind(reason)
            .execute(&mut *db_tx)
            .await?;

            // Append compliance status event
            sqlx::query(
                r#"
                INSERT INTO compliance_status_events (
                    id, compliance_id, investor_id, status, reason
                )
                VALUES ($1, $2, $3, $4, $5)
                "#,
            )
            .bind(Uuid::new_v4())
            .bind(compliance_id)
            .bind(investor_id)
            .bind(ComplianceStatus::Expired.as_str())
            .bind(reason)
            .execute(&mut *db_tx)
            .await?;
        }

        // Outbox — notifies token platform
        let payload = json!({
            "investor_id": investor_id.to_string(),
            "reason":      reason,
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'investor.whitelist_revoked', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(investor_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        warn!(%investor_id, %reason, "investor whitelist revoked");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// A wallet is whitelisted if its most recent event is 'grant'.
    async fn is_wallet_whitelisted(
        &self,
        wallet: &str,
    ) -> Result<bool, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT action
            FROM wallet_whitelist_events
            WHERE wallet_address = $1
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(wallet)
        .fetch_optional(&self.db)
        .await?;

        let whitelisted = row
            .map(|r| {
                r.get::<String, _>("action")
                    == WhitelistAction::Grant.as_str()
            })
            .unwrap_or(false);

        Ok(whitelisted)
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<InvestorCompliance>, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT id, investor_id, wallet_address, tier,
                   jurisdiction, status, kyc_reference,
                   idempotency_key, created_at, expires_at
            FROM investor_compliance
            WHERE idempotency_key = $1
            "#,
        )
        .bind(key)
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| InvestorCompliance {
            id:              r.get("id"),
            investor_id:     r.get("investor_id"),
            wallet_address:  r.get("wallet_address"),
            tier:            r.get("tier"),
            jurisdiction:    r.get("jurisdiction"),
            status:          r.get("status"),
            kyc_reference:   r.get("kyc_reference"),
            idempotency_key: r.get("idempotency_key"),
            created_at:      r.get("created_at"),
            expires_at:      r.get("expires_at"),
        }))
    }

    async fn insert_compliance_and_whitelist(
        &self,
        req: &OnboardInvestorRequest,
        kyc_reference: &str,
        expiry_date: DateTime<Utc>,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        let compliance_id = Uuid::new_v4();

        sqlx::query(
            r#"
            INSERT INTO investor_compliance (
                id, investor_id, wallet_address, tier,
                jurisdiction, status, kyc_reference,
                idempotency_key, created_at, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW(), $9)
            "#,
        )
        .bind(compliance_id)
        .bind(req.investor_id)
        .bind(&req.wallet_address)
        .bind(req.investor_tier.as_str())
        .bind(&req.jurisdiction)
        .bind(ComplianceStatus::Approved.as_str())
        .bind(kyc_reference)
        .bind(&req.idempotency_key)
        .bind(expiry_date)
        .execute(&mut *db_tx)
        .await?;

        // Append whitelist grant event
        sqlx::query(
            r#"
            INSERT INTO wallet_whitelist_events (
                id, investor_id, wallet_address, tier, action
            )
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(req.investor_id)
        .bind(&req.wallet_address)
        .bind(req.investor_tier.as_str())
        .bind(WhitelistAction::Grant.as_str())
        .execute(&mut *db_tx)
        .await?;

        // Outbox — notifies token platform of newly whitelisted wallet
        let payload = json!({
            "investor_id":    req.investor_id.to_string(),
            "wallet_address": req.wallet_address,
            "tier":           req.investor_tier.as_str(),
            "jurisdiction":   req.jurisdiction,
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'investor.whitelisted', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(compliance_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        Ok(compliance_id)
    }
}
