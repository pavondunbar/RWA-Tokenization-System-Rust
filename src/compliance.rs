use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgPool, Row};
use tracing::{info, instrument, warn};
use uuid::Uuid;

use crate::{
    errors::RwaError,
    models::{ComplianceStatus, InvestorCompliance, OnboardInvestorRequest},
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
    ///
    /// RWA tokens are NOT permissionless. Every investor must pass KYC/AML
    /// before holding tokens, and every wallet must be whitelisted.
    /// Both sender AND receiver are checked on every transfer.
    ///
    /// BlackRock BUIDL requirements:
    ///   - Institutional investors only ($5M+ minimum)
    ///   - Full KYC/AML via Securitize
    ///   - Whitelisted Ethereum wallet addresses only
    ///   - KYC expires annually — must re-verify
    #[instrument(skip(self), fields(
        investor_id    = %req.investor_id,
        wallet_address = %req.wallet_address,
        tier           = %req.investor_tier.as_str(),
    ))]
    pub async fn onboard_investor(
        &self,
        req: OnboardInvestorRequest,
    ) -> Result<Uuid, RwaError> {

        // ---- STEP 1: Idempotency check ----------------------------------------
        if let Some(existing) = self.find_by_idempotency_key(&req.idempotency_key).await? {
            info!(
                idempotency_key = %req.idempotency_key,
                compliance_id   = %existing.id,
                "duplicate onboarding — returning existing compliance record"
            );
            return Ok(existing.id);
        }

        // ---- STEP 2: Sanctions screening — OFAC, EU, UN lists ----------------
        // OUTSIDE the DB transaction — never hold a row lock during external calls
        let sanctions_result = self.sanctions_checker
            .screen(req.investor_id, &req.jurisdiction)
            .await?;

        if sanctions_result.is_sanctioned {
            warn!(
                investor_id = %req.investor_id,
                list        = ?sanctions_result.list_name,
                "investor failed sanctions screening"
            );
            return Err(RwaError::SanctionedInvestor { investor_id: req.investor_id });
        }

        // ---- STEP 3: KYC/AML — identity + accreditation verification ---------
        // OUTSIDE the DB transaction — external API call
        let kyc_result = self.kyc_provider
            .verify(req.investor_id, &req.investor_tier)
            .await?;

        if !kyc_result.passed {
            let reason = kyc_result.reason.unwrap_or_else(|| "no reason provided".into());
            warn!(
                investor_id = %req.investor_id,
                %reason,
                "investor failed KYC verification"
            );
            return Err(RwaError::KycFailed { reason });
        }

        // ---- STEP 4: Write compliance record + whitelist wallet atomically ----
        let compliance_id = self
            .insert_compliance_and_whitelist(&req, &kyc_result.reference_id, kyc_result.expiry_date)
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
    /// Called before every token transfer — enforced at the application layer
    /// and optionally mirrored via on-chain oracle or transfer hook.
    pub async fn can_transfer(
        &self,
        from_wallet: &str,
        to_wallet: &str,
    ) -> Result<bool, RwaError> {
        let sender_row = sqlx::query(
            "SELECT id FROM whitelisted_wallets WHERE wallet_address = $1",
        )
        .bind(from_wallet)
        .fetch_optional(&self.db)
        .await?;

        let receiver_row = sqlx::query(
            "SELECT id FROM whitelisted_wallets WHERE wallet_address = $1",
        )
        .bind(to_wallet)
        .fetch_optional(&self.db)
        .await?;

        Ok(sender_row.is_some() && receiver_row.is_some())
    }

    /// Revokes an investor's whitelist status.
    /// Called when KYC expires, a sanctions hit occurs, or investor exits.
    /// After revocation the wallet cannot receive any further transfers.
    #[instrument(skip(self), fields(investor_id = %investor_id))]
    pub async fn revoke_whitelist(
        &self,
        investor_id: Uuid,
        reason: &str,
    ) -> Result<(), RwaError> {
        let mut db_tx = self.db.begin().await?;

        sqlx::query(
            "DELETE FROM whitelisted_wallets WHERE investor_id = $1",
        )
        .bind(investor_id)
        .execute(&mut *db_tx)
        .await?;

        sqlx::query(
            r#"
            UPDATE investor_compliance
            SET status     = $1,
                updated_at = NOW()
            WHERE investor_id = $2
            "#,
        )
        .bind(ComplianceStatus::Expired.as_str())
        .bind(investor_id)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — notifies token platform to immediately block further transfers
        let payload = json!({
            "investor_id": investor_id.to_string(),
            "reason":      reason,
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
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

        // Whitelist the wallet — only whitelisted wallets can send or receive tokens
        sqlx::query(
            r#"
            INSERT INTO whitelisted_wallets (
                id, investor_id, wallet_address, tier, created_at
            )
            VALUES ($1, $2, $3, $4, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(req.investor_id)
        .bind(&req.wallet_address)
        .bind(req.investor_tier.as_str())
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
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
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
