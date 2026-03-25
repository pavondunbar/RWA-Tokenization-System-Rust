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
        LedgerAccount, MintSigningMessage, MintStatus,
        MintTokensRequest, TokenMint,
    },
    ports::MintSigningQueue,
};

pub struct TokenMintingService {
    db:           PgPool,
    compliance:   Arc<KycComplianceService>,
    signing_queue: Box<dyn MintSigningQueue>,
}

impl TokenMintingService {
    pub fn new(
        db: PgPool,
        compliance: Arc<KycComplianceService>,
        signing_queue: Box<dyn MintSigningQueue>,
    ) -> Self {
        Self { db, compliance, signing_queue }
    }

    /// Mints RWA tokens to a whitelisted investor wallet.
    #[instrument(skip(self), fields(
        asset_id       = %req.asset_id,
        investor_id    = %req.investor_id,
        wallet_address = %req.wallet_address,
        token_amount   = %req.token_amount,
    ))]
    pub async fn mint_tokens(
        &self,
        req: MintTokensRequest,
    ) -> Result<TokenMint, RwaError> {

        if let Some(existing) = self
            .find_by_idempotency_key(&req.idempotency_key)
            .await?
        {
            info!(
                idempotency_key = %req.idempotency_key,
                mint_id         = %existing.id,
                "duplicate mint request — returning existing record"
            );
            return Ok(existing);
        }

        let can_receive = self.compliance
            .can_transfer("ISSUER_WALLET", &req.wallet_address)
            .await?;

        if !can_receive {
            return Err(RwaError::WalletNotWhitelisted {
                wallet: req.wallet_address.clone(),
            });
        }

        let mint_id = self
            .lock_supply_and_create_mint(&req)
            .await?;

        let mint = self.find_mint_by_id(mint_id).await?;

        self.signing_queue
            .send_mint(
                MintSigningMessage {
                    mint_id:      mint_id.to_string(),
                    asset_id:     req.asset_id.to_string(),
                    wallet:       req.wallet_address.clone(),
                    token_amount: req.token_amount.to_string(),
                    action:       "mint".into(),
                },
                req.asset_id.to_string(),
                mint_id.to_string(),
            )
            .await?;

        info!(%mint_id, "tokens enqueued for on-chain minting");
        Ok(mint)
    }

    /// Called by ConfirmationTracker when the mint transaction
    /// is confirmed on-chain.
    #[instrument(skip(self), fields(%mint_id, %tx_hash, %block_number))]
    pub async fn confirm_mint(
        &self,
        mint_id: Uuid,
        tx_hash: &str,
        block_number: i64,
    ) -> Result<(), RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Read mint details for ledger entries
        let row = sqlx::query(
            r#"
            SELECT asset_id, investor_id, token_amount, fiat_received
            FROM token_mints
            WHERE id = $1
            "#,
        )
        .bind(mint_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::MintNotFound { mint_id })?;

        let asset_id: Uuid = row.get("asset_id");
        let investor_id: Uuid = row.get("investor_id");
        let token_amount: Decimal = row.get("token_amount");
        let fiat_received: Decimal = row.get("fiat_received");

        // Append confirmation event instead of UPDATE
        sqlx::query(
            r#"
            INSERT INTO token_mint_events (
                id, mint_id, status, tx_hash, block_number
            )
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id)
        .bind(MintStatus::Confirmed.as_str())
        .bind(tx_hash)
        .bind(block_number)
        .execute(&mut *db_tx)
        .await?;

        // Double-entry ledger: token mint
        // DEBIT investor:tokens (investor receives tokens)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.mint', $2, $3, $4, $5, 0, 'TOKEN',
                    'tokens minted to investor')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id)
        .bind(LedgerAccount::investor_tokens(investor_id))
        .bind(asset_id)
        .bind(token_amount)
        .execute(&mut *db_tx)
        .await?;

        // CREDIT fund:treasury (fund issues tokens)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.mint', $2, $3, $4, 0, $5, 'TOKEN',
                    'tokens issued from fund treasury')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id)
        .bind(LedgerAccount::fund_treasury(asset_id))
        .bind(asset_id)
        .bind(token_amount)
        .execute(&mut *db_tx)
        .await?;

        // DEBIT fund:fiat (fund receives fiat)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.mint', $2, $3, $4, $5, 0, 'USD',
                    'fiat received from investor')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id)
        .bind(LedgerAccount::fund_fiat(asset_id))
        .bind(asset_id)
        .bind(fiat_received)
        .execute(&mut *db_tx)
        .await?;

        // CREDIT investor:fiat (investor pays fiat)
        sqlx::query(
            r#"
            INSERT INTO ledger_entries (
                id, event_type, reference_id, account,
                asset_id, debit, credit, currency, memo
            )
            VALUES ($1, 'token.mint', $2, $3, $4, 0, $5, 'USD',
                    'fiat paid by investor')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id)
        .bind(LedgerAccount::investor_fiat(investor_id))
        .bind(asset_id)
        .bind(fiat_received)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — notifies investor dashboard
        let payload = json!({
            "mint_id":      mint_id.to_string(),
            "tx_hash":      tx_hash,
            "block_number": block_number,
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'token.mint_confirmed', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(
            %mint_id, %tx_hash, %block_number,
            "mint confirmed on-chain"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn lock_supply_and_create_mint(
        &self,
        req: &MintTokensRequest,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Pessimistic lock on legal_wrappers
        let wrapper_row = sqlx::query(
            r#"
            SELECT token_supply
            FROM legal_wrappers
            WHERE asset_id = $1
            FOR UPDATE
            "#,
        )
        .bind(req.asset_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::SupplyNotFound {
            asset_id: req.asset_id,
        })?;

        let total_supply: Decimal =
            wrapper_row.get("token_supply");

        // All mints count (pending + confirmed) — supply is
        // reserved at creation
        let minted_row = sqlx::query(
            r#"
            SELECT COALESCE(SUM(token_amount), 0) AS total
            FROM token_mints
            WHERE asset_id = $1
            "#,
        )
        .bind(req.asset_id)
        .fetch_one(&mut *db_tx)
        .await?;
        let total_minted: Decimal = minted_row.get("total");

        // Only settled redemptions release supply.
        // Status is now derived from token_redemption_events.
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
        .bind(req.asset_id)
        .fetch_one(&mut *db_tx)
        .await?;
        let total_redeemed: Decimal = redeemed_row.get("total");

        let allocated = total_minted - total_redeemed;
        let available = total_supply - allocated;

        if available < req.token_amount {
            return Err(RwaError::InsufficientTokenSupply {
                available,
                requested: req.token_amount,
            });
        }

        let mint_id = Uuid::new_v4();

        sqlx::query(
            r#"
            INSERT INTO token_mints (
                id, asset_id, investor_id, wallet_address,
                token_amount, fiat_received, status,
                idempotency_key, created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
            "#,
        )
        .bind(mint_id)
        .bind(req.asset_id)
        .bind(req.investor_id)
        .bind(&req.wallet_address)
        .bind(req.token_amount)
        .bind(req.fiat_received)
        .bind(MintStatus::Pending.as_str())
        .bind(&req.idempotency_key)
        .execute(&mut *db_tx)
        .await?;

        let payload = json!({
            "mint_id":       mint_id.to_string(),
            "asset_id":      req.asset_id.to_string(),
            "wallet":        req.wallet_address,
            "token_amount":  req.token_amount.to_string(),
            "fiat_received": req.fiat_received.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events
                (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'token.mint_requested', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(
            %mint_id,
            asset_id     = %req.asset_id,
            token_amount = %req.token_amount,
            "token supply locked and mint record created"
        );

        Ok(mint_id)
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<TokenMint>, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT id, asset_id, investor_id, wallet_address,
                   token_amount, fiat_received, status,
                   idempotency_key, tx_hash, block_number,
                   created_at, confirmed_at
            FROM token_mints
            WHERE idempotency_key = $1
            "#,
        )
        .bind(key)
        .fetch_optional(&self.db)
        .await?;

        Ok(row.map(|r| map_mint_row(&r)))
    }

    async fn find_mint_by_id(
        &self,
        mint_id: Uuid,
    ) -> Result<TokenMint, RwaError> {
        let row = sqlx::query(
            r#"
            SELECT id, asset_id, investor_id, wallet_address,
                   token_amount, fiat_received, status,
                   idempotency_key, tx_hash, block_number,
                   created_at, confirmed_at
            FROM token_mints
            WHERE id = $1
            "#,
        )
        .bind(mint_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(RwaError::MintNotFound { mint_id })?;

        Ok(map_mint_row(&row))
    }
}

// -----------------------------------------------------------------------
// Row mapping helper
// -----------------------------------------------------------------------

pub fn map_mint_row(row: &sqlx::postgres::PgRow) -> TokenMint {
    TokenMint {
        id:              row.get("id"),
        asset_id:        row.get("asset_id"),
        investor_id:     row.get("investor_id"),
        wallet_address:  row.get("wallet_address"),
        token_amount:    row.get("token_amount"),
        fiat_received:   row.get("fiat_received"),
        status:          row.get("status"),
        idempotency_key: row.get("idempotency_key"),
        tx_hash:         row.get("tx_hash"),
        block_number:    row.get("block_number"),
        created_at:      row.get("created_at"),
        confirmed_at:    row.get("confirmed_at"),
    }
}
