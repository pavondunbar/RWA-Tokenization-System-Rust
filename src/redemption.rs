use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    compliance::KycComplianceService,
    errors::RwaError,
    models::{BurnSigningMessage, MintStatus, RedemptionRequest, RedemptionStatus, TokenRedemption},
    ports::MintSigningQueue,
};

pub struct TokenRedemptionService {
    db:           PgPool,
    compliance:   Arc<KycComplianceService>,
    signing_queue: Box<dyn MintSigningQueue>,
}

impl TokenRedemptionService {
    pub fn new(
        db: PgPool,
        compliance: Arc<KycComplianceService>,
        signing_queue: Box<dyn MintSigningQueue>,
    ) -> Self {
        Self { db, compliance, signing_queue }
    }

    /// Initiates token redemption. Investor returns tokens; they are burned
    /// on-chain and fiat is wired back.
    ///
    /// This is the most operationally complex flow — it spans blockchain (burn),
    /// compliance (whitelist check), and TradFi (wire transfer). Each step
    /// must succeed, or the prior steps must roll back.
    ///
    /// State machine: PENDING → APPROVED → BURNING → BURNED → SETTLED
    #[instrument(skip(self), fields(
        asset_id       = %req.asset_id,
        investor_id    = %req.investor_id,
        wallet_address = %req.wallet_address,
        token_amount   = %req.token_amount,
    ))]
    pub async fn request_redemption(
        &self,
        req: RedemptionRequest,
    ) -> Result<Uuid, RwaError> {

        // ---- STEP 1: Idempotency check ----------------------------------------
        if let Some(existing) = self.find_by_idempotency_key(&req.idempotency_key).await? {
            info!(
                idempotency_key = %req.idempotency_key,
                redemption_id   = %existing.id,
                "duplicate redemption request — returning existing record"
            );
            return Ok(existing.id);
        }

        // ---- STEP 2: Whitelist check — investor must still be KYC-approved ----
        // OUTSIDE the DB transaction — read-only compliance check
        let can_send = self.compliance
            .can_transfer(&req.wallet_address, "ISSUER_WALLET")
            .await?;

        if !can_send {
            return Err(RwaError::WalletNotWhitelisted {
                wallet: req.wallet_address.clone(),
            });
        }

        // ---- STEP 3: Lock balance + create redemption record + outbox ---------
        let redemption_id = self.lock_balance_and_create_redemption(&req).await?;

        // ---- STEP 4: Enqueue burn OUTSIDE the DB transaction -----------------
        // asset_id as group ID → per-asset FIFO ordering.
        // redemption_id as dedup ID → idempotent enqueue on retries.
        self.signing_queue
            .send_burn(
                BurnSigningMessage {
                    redemption_id: redemption_id.to_string(),
                    asset_id:      req.asset_id.to_string(),
                    wallet:        req.wallet_address.clone(),
                    token_amount:  req.token_amount.to_string(),
                    action:        "burn".into(),
                },
                req.asset_id.to_string(),      // MessageGroupId
                redemption_id.to_string(),     // MessageDeduplicationId
            )
            .await?;

        info!(%redemption_id, "redemption enqueued for on-chain burn");
        Ok(redemption_id)
    }

    /// Called after the burn is confirmed on-chain.
    /// Releases the minted_supply counter and triggers the fiat wire.
    /// This is the final step — state machine: BURNED → SETTLED.
    #[instrument(skip(self), fields(%redemption_id))]
    pub async fn settle_redemption(
        &self,
        redemption_id: Uuid,
    ) -> Result<(), RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Lock the redemption record — one settlement at a time
        let row = sqlx::query(
            r#"
            SELECT id, asset_id, investor_id, wallet_address,
                   token_amount, bank_account, status,
                   idempotency_key, created_at, settled_at
            FROM token_redemptions
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(redemption_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::RedemptionNotFound { redemption_id })?;

        let status: String      = row.get("status");
        let asset_id: Uuid      = row.get("asset_id");
        let token_amount: Decimal = row.get("token_amount");
        let investor_id: Uuid   = row.get("investor_id");
        let bank_account: String = row.get("bank_account");

        // State machine guard — must be BURNED before settling
        if status != RedemptionStatus::Burned.as_str() {
            return Err(RwaError::InvalidState {
                expected: RedemptionStatus::Burned.as_str().into(),
                got:      status,
            });
        }

        // Release tokens from supply — they have been burned on-chain
        sqlx::query(
            r#"
            UPDATE rwa_token_supply
            SET minted_supply = minted_supply - $1,
                updated_at    = NOW()
            WHERE asset_id = $2
            "#,
        )
        .bind(token_amount)
        .bind(asset_id)
        .execute(&mut *db_tx)
        .await?;

        // Mark redemption as settled
        sqlx::query(
            r#"
            UPDATE token_redemptions
            SET status     = $1,
                settled_at = NOW()
            WHERE id = $2
            "#,
        )
        .bind(RedemptionStatus::Settled.as_str())
        .bind(redemption_id)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — triggers the fiat wire transfer to investor bank account
        let payload = json!({
            "redemption_id": redemption_id.to_string(),
            "investor_id":   investor_id.to_string(),
            "bank_account":  bank_account,
            "token_amount":  token_amount.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'token.redemption_settled', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(
            %redemption_id,
            %investor_id,
            %token_amount,
            "redemption settled — fiat wire triggered"
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Acquires a pessimistic lock on the investor's confirmed token_mint balance,
    /// checks it covers the redemption amount, creates the redemption record,
    /// and writes the outbox event — all atomically.
    async fn lock_balance_and_create_redemption(
        &self,
        req: &RedemptionRequest,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Lock investor's confirmed balance for this asset
        let balance_row = sqlx::query(
            r#"
            SELECT SUM(token_amount) AS total_balance
            FROM token_mints
            WHERE investor_id = $1
              AND asset_id    = $2
              AND status      = $3
            FOR UPDATE
            "#,
        )
        .bind(req.investor_id)
        .bind(req.asset_id)
        .bind(MintStatus::Confirmed.as_str())
        .fetch_one(&mut *db_tx)
        .await?;

        let total_balance: Option<Decimal> = balance_row.get("total_balance");
        let held = total_balance.unwrap_or(Decimal::ZERO);

        if held < req.token_amount {
            return Err(RwaError::InsufficientTokenBalance {
                available: held,
                requested: req.token_amount,
            });
        }

        let redemption_id = Uuid::new_v4();

        sqlx::query(
            r#"
            INSERT INTO token_redemptions (
                id, asset_id, investor_id, wallet_address,
                token_amount, bank_account, status,
                idempotency_key, created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
            "#,
        )
        .bind(redemption_id)
        .bind(req.asset_id)
        .bind(req.investor_id)
        .bind(&req.wallet_address)
        .bind(req.token_amount)
        .bind(&req.bank_account)
        .bind(RedemptionStatus::Pending.as_str())
        .bind(&req.idempotency_key)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — triggers burn workflow
        let payload = json!({
            "redemption_id": redemption_id.to_string(),
            "asset_id":      req.asset_id.to_string(),
            "wallet":        req.wallet_address,
            "token_amount":  req.token_amount.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'token.redemption_requested', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(
            %redemption_id,
            investor_id  = %req.investor_id,
            token_amount = %req.token_amount,
            "balance locked and redemption record created"
        );

        Ok(redemption_id)
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<TokenRedemption>, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT id, asset_id, investor_id, wallet_address,
                   token_amount, bank_account, status,
                   idempotency_key, created_at, settled_at
            FROM token_redemptions
            WHERE idempotency_key = $1
            "#,
        )
        .bind(key)
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| TokenRedemption {
            id:              r.get("id"),
            asset_id:        r.get("asset_id"),
            investor_id:     r.get("investor_id"),
            wallet_address:  r.get("wallet_address"),
            token_amount:    r.get("token_amount"),
            bank_account:    r.get("bank_account"),
            status:          r.get("status"),
            idempotency_key: r.get("idempotency_key"),
            created_at:      r.get("created_at"),
            settled_at:      r.get("settled_at"),
        }))
    }
}
