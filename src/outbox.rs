use sqlx::{PgPool, Row};
use tokio::time::{sleep, Duration};
use tracing::{error, info, instrument};
use uuid::Uuid;

use crate::{errors::RwaError, ports::KafkaProducer};

pub struct RwaOutboxPublisher {
    db:         PgPool,
    kafka:      Box<dyn KafkaProducer>,
    batch_size: i64,
}

impl RwaOutboxPublisher {
    pub fn new(
        db: PgPool,
        kafka: Box<dyn KafkaProducer>,
        batch_size: i64,
    ) -> Self {
        Self { db, kafka, batch_size }
    }

    /// Polls `outbox_events` for unpublished events and delivers
    /// them to Kafka. Unpublished = no matching row in
    /// `outbox_publish_log`.
    #[instrument(skip(self))]
    pub async fn poll_and_publish(
        &self,
    ) -> Result<usize, RwaError> {
        let rows = sqlx::query(
            r#"
            SELECT oe.id, oe.aggregate_id,
                   oe.event_type, oe.payload
            FROM outbox_events oe
            LEFT JOIN outbox_publish_log opl
                ON opl.event_id = oe.id
            WHERE opl.id IS NULL
            ORDER BY oe.created_at
            LIMIT $1
            "#,
        )
        .bind(self.batch_size)
        .fetch_all(&self.db)
        .await?;

        if rows.is_empty() {
            return Ok(0);
        }

        let mut published = 0usize;

        for row in &rows {
            let event_id: Uuid = row.get("id");
            let aggregate_id: String = row.get("aggregate_id");
            let event_type: String = row.get("event_type");
            let payload: serde_json::Value = row.get("payload");

            let mut db_tx = self.db.begin().await?;

            // Re-lock within a per-event transaction.
            // SKIP LOCKED prevents concurrent publishers
            // from claiming the same event.
            let locked = sqlx::query(
                r#"
                SELECT oe.id FROM outbox_events oe
                LEFT JOIN outbox_publish_log opl
                    ON opl.event_id = oe.id
                WHERE oe.id = $1 AND opl.id IS NULL
                FOR UPDATE OF oe SKIP LOCKED
                "#,
            )
            .bind(event_id)
            .fetch_optional(&mut *db_tx)
            .await?;

            if locked.is_none() {
                continue;
            }

            let payload_bytes = payload.to_string();

            if let Err(e) = self.kafka
                .send(
                    &event_type,
                    aggregate_id.as_bytes(),
                    payload_bytes.as_bytes(),
                )
                .await
            {
                error!(
                    %event_id, error = %e,
                    "kafka send failed — will retry next poll"
                );
                break;
            }

            // Append publish record instead of UPDATE
            sqlx::query(
                r#"
                INSERT INTO outbox_publish_log (id, event_id)
                VALUES ($1, $2)
                "#,
            )
            .bind(Uuid::new_v4())
            .bind(event_id)
            .execute(&mut *db_tx)
            .await?;

            db_tx.commit().await?;

            published += 1;
        }

        info!(
            batch_size = published,
            "outbox events published to Kafka"
        );
        Ok(published)
    }

    /// Runs the publisher loop indefinitely.
    pub async fn run_forever(&self, poll_interval_ms: u64) {
        info!(poll_interval_ms, "outbox publisher started");
        loop {
            match self.poll_and_publish().await {
                Ok(0) => {}
                Ok(n) => info!(
                    published = n, "outbox batch complete"
                ),
                Err(e) => error!(
                    error = %e, "outbox poll failed — will retry"
                ),
            }
            sleep(Duration::from_millis(poll_interval_ms)).await;
        }
    }
}
