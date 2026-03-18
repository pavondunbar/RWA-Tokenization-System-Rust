use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    compliance::KycComplianceService,
    errors::RwaError,
    models::{MintSigningMessage, MintStatus, MintTokensRequest, TokenMint},
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
    /// Triggered when investor wires fiat to the fund.
    ///
    /// BlackRock BUIDL example:
    ///   - Investor wires $10M USD to fund
    ///   - Fund receives fiat, buys T-bills with proceeds
    ///   - Mints 10,000,000 BUIDL tokens ($1.00 NAV each)
    ///   - Yield accrues by rebasing token balance daily
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

        // ---- STEP 1: Idempotency check ----------------------------------------
        if let Some(existing) = self.find_by_idempotency_key(&req.idempotency_key).await? {
            info!(
                idempotency_key = %req.idempotency_key,
                mint_id         = %existing.id,
                "duplicate mint request — returning existing record"
            );
            return Ok(existing);
        }

        // ---- STEP 2: Wallet whitelist check -----------------------------------
        // OUTSIDE the DB transaction — read-only compliance check
        let can_receive = self.compliance
            .can_transfer("ISSUER_WALLET", &req.wallet_address)
            .await?;

        if !can_receive {
            return Err(RwaError::WalletNotWhitelisted {
                wallet: req.wallet_address.clone(),
            });
        }

        // ---- STEP 3: Lock supply + create mint record + outbox ---------------
        let mint_id = self.lock_supply_and_create_mint(&req).await?;

        // ---- STEP 4: Enqueue for signing OUTSIDE the DB transaction ----------
        // Never hold a row lock during external queue calls.
        // asset_id as group ID → per-asset FIFO ordering in SQS FIFO queue.
        // mint_id as dedup ID  → idempotent enqueue on transient errors.
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
                req.asset_id.to_string(),  // MessageGroupId
                mint_id.to_string(),       // MessageDeduplicationId
            )
            .await?;

        info!(%mint_id, "tokens enqueued for on-chain minting");
        Ok(mint)
    }

    /// Called by ConfirmationTracker when the mint transaction is confirmed on-chain.
    /// Settles the ledger — marks confirmed and records the on-chain proof.
    #[instrument(skip(self), fields(%mint_id, %tx_hash, %block_number))]
    pub async fn confirm_mint(
        &self,
        mint_id: Uuid,
        tx_hash: &str,
        block_number: i64,
    ) -> Result<(), RwaError> {
        let mut db_tx = self.db.begin().await?;

        sqlx::query(
            r#"
            UPDATE token_mints
            SET status       = $1,
                tx_hash      = $2,
                block_number = $3,
                confirmed_at = NOW()
            WHERE id = $4
            "#,
        )
        .bind(MintStatus::Confirmed.as_str())
        .bind(tx_hash)
        .bind(block_number)
        .bind(mint_id)
        .execute(&mut *db_tx)
        .await?;

        // Outbox — notifies investor dashboard and downstream systems
        let payload = json!({
            "mint_id":      mint_id.to_string(),
            "tx_hash":      tx_hash,
            "block_number": block_number,
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
            VALUES ($1, $2, 'token.mint_confirmed', $3, NOW())
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(mint_id.to_string())
        .bind(payload)
        .execute(&mut *db_tx)
        .await?;

        db_tx.commit().await?;

        info!(%mint_id, %tx_hash, %block_number, "mint confirmed on-chain");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Acquires a pessimistic lock on rwa_token_supply, checks available supply,
    /// increments minted_supply, inserts the token_mint record, and writes the
    /// outbox event — all in a single DB transaction.
    async fn lock_supply_and_create_mint(
        &self,
        req: &MintTokensRequest,
    ) -> Result<Uuid, RwaError> {
        let mut db_tx = self.db.begin().await?;

        // Pessimistic lock — serialises concurrent mints for this asset.
        // Prevents overselling: two simultaneous $250M mints against a
        // $500M fund cannot both succeed if only $300M supply remains.
        let supply_row = sqlx::query(
            r#"
            SELECT id, total_supply, minted_supply
            FROM rwa_token_supply
            WHERE asset_id = $1
            FOR UPDATE
            "#,
        )
        .bind(req.asset_id)
        .fetch_optional(&mut *db_tx)
        .await?
        .ok_or(RwaError::SupplyNotFound { asset_id: req.asset_id })?;

        let total_supply: Decimal  = supply_row.get("total_supply");
        let minted_supply: Decimal = supply_row.get("minted_supply");
        let available = total_supply - minted_supply;

        if available < req.token_amount {
            return Err(RwaError::InsufficientTokenSupply {
                available,
                requested: req.token_amount,
            });
        }

        // Reserve tokens — incremented now, locked until on-chain confirmation
        sqlx::query(
            r#"
            UPDATE rwa_token_supply
            SET minted_supply = minted_supply + $1,
                updated_at    = NOW()
            WHERE asset_id = $2
            "#,
        )
        .bind(req.token_amount)
        .bind(req.asset_id)
        .execute(&mut *db_tx)
        .await?;

        // Create mint record — state machine entry point
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

        // Outbox event in the same transaction — durable even if broker is down
        let payload = json!({
            "mint_id":       mint_id.to_string(),
            "asset_id":      req.asset_id.to_string(),
            "wallet":        req.wallet_address,
            "token_amount":  req.token_amount.to_string(),
            "fiat_received": req.fiat_received.to_string(),
        });

        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, aggregate_id, event_type, payload, created_at)
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

    async fn find_mint_by_id(&self, mint_id: Uuid) -> Result<TokenMint, RwaError> {
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
