use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    compliance::KycComplianceService,
    errors::RwaError,
    models::{
        BurnSigningMessage, LedgerAccount, RedemptionRequest,
        RedemptionStatus, TokenRedemption,
    },
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

    /// Initiates token redemption.
    /// State machine: PENDING -> APPROVED -> BURNING -> BURNED -> SETTLED
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

        if let Some(existing) = self
            .find_by_idempotency_key(&req.idempotency_key)
            .await?
        {
            info!(
                idempotency_key = %req.idempotency_key,
                redemption_id   = %existing.id,
                "duplicate redemption — returning existing record"
            );
            return Ok(existing.id);
        }

        let can_send = self.compliance
            .can_transfer(&req.wallet_address, "ISSUER_WALLET")
            .await?;

        if !can_send {
            return Err(RwaError::WalletNotWhitelisted {
                wallet: req.wallet_address.clone(),
            });
        }

        let redemption_id = self
            .lock_balance_and_create_redemption(&req)
            .await?;

        self.signing_queue
            .send_burn(
                BurnSigningMessage {
                    redemption_id: redemption_id.to_string(),
                    asset_id:      req.asset_id.to_string(),
                    wallet:        req.wallet_address.clone(),
                    token_amount:  req.token_amount.to_string(),
                    action:        "burn".into(),
                },
                req.asset_id.to_string(),
                redemption_id.to_string(),
            )
            .await?;

        info!(
            %redemption_id,
            "redemption enqueued for on-chain burn"
        );
        Ok(redemption_id)
    }

    /// Called after the burn is confirmed on-chain.
    /// State machine: BURNED -> SETTLED.
    #[instrument(skip(self), fields(%redemption_id))]
    pub async fn settle_redemption(
        &self,
        redemption_id: Uuid,
    ) -> Result<(), RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Read redemption details (immutable row)
        let row = sqlx::query(
            r#"
            SELECT id, asset_id, investor_id, wallet_address,
                   token_amount, bank_account
            FROM token_redemptions
            WHERE id = $1
            "#,
        )
        .bind(redemption_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::RedemptionNotFound { redemption_id })?;

        let asset_id: Uuid = row.get("asset_id");
        let investor_id: Uuid = row.get("investor_id");
        let token_amount: Decimal = row.get("token_amount");
        let bank_account: String = row.get("bank_account");

        // Read latest status from event log
        let status_row = sqlx::query(
            r#"
            SELECT status
            FROM token_redemption_events
            WHERE redemption_id = $1
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(redemption_id)
        .fetch_optional(&mut *db_tx)
        .await?;

        let status = status_row
            .map(|r| r.get::<String, _>("status"))
            .unwrap_or_else(|| {
                // No events yet means initial "pending" status
                RedemptionStatus::Pending.as_str().to_string()
            });

        if status != RedemptionStatus::Burned.as_str() {
            return Err(RwaError::InvalidState {
                expected: RedemptionStatus::Burned.as_str().into(),
                got:      status,
            });
        }

        // Append settlement event instead of UPDATE
        sqlx::query(
            r#"
            INSERT INTO token_redemption_events (
                id, redemption_id, status
            )
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id)
        .bind(RedemptionStatus::Settled.as_str())
        .execute(&mut *db_tx)
        .await?;

        // Double-entry ledger: redemption settlement
        // DEBIT fund:treasury (fund receives tokens back)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.redeem', $2, $3, $4, $5, 0, 'TOKEN',
                    'tokens returned to fund treasury')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id)
        .bind(LedgerAccount::fund_treasury(asset_id))
        .bind(asset_id)
        .bind(token_amount)
        .execute(&mut *db_tx)
        .await?;

        // CREDIT investor:tokens (investor returns tokens)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.redeem', $2, $3, $4, 0, $5, 'TOKEN',
                    'tokens redeemed by investor')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id)
        .bind(LedgerAccount::investor_tokens(investor_id))
        .bind(asset_id)
        .bind(token_amount)
        .execute(&mut *db_tx)
        .await?;

        // DEBIT investor:fiat (investor receives fiat)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.redeem', $2, $3, $4, $5, 0, 'USD',
                    'fiat paid to investor')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id)
        .bind(LedgerAccount::investor_fiat(investor_id))
        .bind(asset_id)
        .bind(token_amount) // 1:1 token-to-fiat at $1.00
        .execute(&mut *db_tx)
        .await?;

        // CREDIT fund:fiat (fund pays fiat)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.redeem', $2, $3, $4, 0, $5, 'USD',
                    'fiat disbursed from fund')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(redemption_id)
        .bind(LedgerAccount::fund_fiat(asset_id))
        .bind(asset_id)
        .bind(token_amount)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — triggers the fiat wire transfer
        let payload = json!({
            "redemption_id": redemption_id.to_string(),
            "investor_id":   investor_id.to_string(),
            "bank_account":  bank_account,
            "token_amount":  token_amount.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
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

    async fn lock_balance_and_create_redemption(
        &self,
        req: &RedemptionRequest,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Lock investor's confirmed mint rows then sum.
        // Confirmed status is now derived from token_mint_events.
        sqlx::query(
            r#"
            SELECT tm.id
            FROM token_mints tm
            WHERE tm.investor_id = $1
              AND tm.asset_id    = $2
              AND EXISTS (
                SELECT 1 FROM token_mint_events tme
                WHERE tme.mint_id = tm.id
                  AND tme.status = 'confirmed'
              )
            FOR UPDATE
            "#,
        )
        .bind(req.investor_id)
        .bind(req.asset_id)
        .fetch_all(&mut *db_tx)
        .await?;

        let balance_row = sqlx::query(
            r#"
            SELECT COALESCE(SUM(tm.token_amount), 0)
                AS total_balance
            FROM token_mints tm
            WHERE tm.investor_id = $1
              AND tm.asset_id    = $2
              AND EXISTS (
                SELECT 1 FROM token_mint_events tme
                WHERE tme.mint_id = tm.id
                  AND tme.status = 'confirmed'
              )
            "#,
        )
        .bind(req.investor_id)
        .bind(req.asset_id)
        .fetch_one(&mut *db_tx)
        .await?;

        let held: Decimal = balance_row.get("total_balance");

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

        let payload = json!({
            "redemption_id": redemption_id.to_string(),
            "asset_id":      req.asset_id.to_string(),
            "wallet":        req.wallet_address,
            "token_amount":  req.token_amount.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
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
