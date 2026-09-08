//! `OutboxDispatcher`: drains the `outbox` table through `POST /v1/groups/{id}/messages`
//! with exponential backoff, stale in-flight recovery and a wake-up signal.

use crate::backoff::backoff_delay;
use crate::constants::{FALLBACK_POLL, IN_FLIGHT_STALE};
use crate::error::{AppError, Result};
use crate::http::TzibburClient;
use crate::models::{now_epoch_ms, MessageDto};
use crate::store::{LocalStore, MessageEntity, OutboxEntity, OutboxState};
use crate::validation::{validate_message_body, TextValidation};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;

/// One iteration of the dispatch loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A message was dispatched; loop again immediately.
    Processed,
    /// Nothing pending; wait for a `poke()`.
    Idle,
    /// Next attempt scheduled at this epoch-ms instant.
    WaitUntil(i64),
}

/// Notifications from the dispatcher.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboxEvent {
    Confirmed {
        client_message_id: String,
        message: MessageDto,
    },
    /// Permanent failure; row stays `Failed` until retried by the user.
    Rejected {
        client_message_id: String,
        code: String,
    },
    /// Transient failure; will retry at `next_at`.
    Rescheduled {
        client_message_id: String,
        next_at: i64,
        attempt: i64,
        code: String,
    },
}

pub struct OutboxDispatcher {
    client: TzibburClient,
    store: Arc<dyn LocalStore>,
    wake: Arc<Notify>,
    events: broadcast::Sender<OutboxEvent>,
    task: Mutex<Option<(JoinHandle<()>, watch::Sender<bool>)>>,
}

impl OutboxDispatcher {
    pub fn new(client: TzibburClient, store: Arc<dyn LocalStore>) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            client,
            store,
            wake: Arc::new(Notify::new()),
            events,
            task: Mutex::new(None),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<OutboxEvent> {
        self.events.subscribe()
    }

    /// Validate and enqueue a message; wakes the loop. Returns the outbox row
    /// (its `client_message_id` is the UUID sent to the server).
    pub fn enqueue(&self, group_id: &str, body: &str) -> Result<OutboxEntity> {
        let body = match validate_message_body(body) {
            TextValidation::Valid { text } => text,
            TextValidation::Empty => {
                return Err(AppError::InvalidInput("message body is empty".into()))
            }
            TextValidation::TooLong { count, max } => {
                return Err(AppError::InvalidInput(format!(
                    "message body has {count} code points, max {max}"
                )))
            }
        };
        let entity = OutboxEntity::new(group_id, &body);
        self.store.insert_outbox(&entity)?;
        self.poke();
        Ok(entity)
    }

    /// Wake the loop (a `Channel<Unit>` send in the original).
    pub fn poke(&self) {
        self.wake.notify_one();
    }

    /// Clear a failed row's error and dispatch it again.
    pub fn retry(&self, client_message_id: &str) -> Result<()> {
        self.store.retry_outbox(client_message_id)?;
        self.poke();
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.task.lock().is_some()
    }

    pub fn start(self: &Arc<Self>) {
        let mut task = self.task.lock();
        if task.is_some() {
            return;
        }
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let me = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let step = match me.step().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "outbox: step failed");
                        Step::WaitUntil(now_epoch_ms() + FALLBACK_POLL.as_millis() as i64)
                    }
                };
                let wait = match step {
                    Step::Processed => continue,
                    Step::Idle => FALLBACK_POLL,
                    Step::WaitUntil(at) => {
                        let ms = (at - now_epoch_ms()).max(0) as u64;
                        Duration::from_millis(ms).min(FALLBACK_POLL)
                    }
                };
                tokio::select! {
                    _ = me.wake.notified() => {}
                    _ = tokio::time::sleep(wait) => {}
                    _ = stop_rx.changed() => break,
                }
            }
        });
        *task = Some((handle, stop_tx));
    }

    pub async fn stop(&self) {
        let taken = self.task.lock().take();
        if let Some((handle, stop_tx)) = taken {
            let _ = stop_tx.send(true);
            let _ = handle.await;
        }
    }

    /// Dispatch at most one row.
    pub async fn step(&self) -> Result<Step> {
        let now = now_epoch_ms();
        let stale_before = now - IN_FLIGHT_STALE.as_millis() as i64;
        let Some(row) = self.store.next_dispatchable(now, stale_before)? else {
            return Ok(match self.store.earliest_next_attempt(now)? {
                Some(at) => Step::WaitUntil(at),
                None => Step::Idle,
            });
        };
        if row.state == OutboxState::InFlight {
            tracing::info!(id = %row.client_message_id, "outbox: re-dispatching stale IN_FLIGHT row");
        }
        self.store.mark_in_flight(&row.client_message_id, now)?;

        match self
            .client
            .send_message(&row.group_id, &row.client_message_id, &row.body)
            .await
        {
            Ok(msg) => {
                // Store through the reconciler so the echo path confirms the row; then make sure.
                let entity = MessageEntity::from_dto(msg.clone(), &row.group_id);
                let _ = self
                    .store
                    .store_incoming_batch(&row.group_id, vec![entity])?;
                self.store
                    .confirm_sent(&row.client_message_id, &msg.id, msg.seq)?;
                let _ = self.events.send(OutboxEvent::Confirmed {
                    client_message_id: row.client_message_id,
                    message: msg,
                });
            }
            Err(AppError::ClientMessageIdReused { .. }) => {
                // Server already has it; the WS echo / pending catch-up will confirm.
                match self.store.get_outbox(&row.client_message_id)? {
                    Some(r) if r.state == OutboxState::Confirmed => {}
                    _ => {
                        self.store
                            .mark_rejected(&row.client_message_id, "client-message-id-reused")?;
                        let _ = self.events.send(OutboxEvent::Rejected {
                            client_message_id: row.client_message_id,
                            code: "client-message-id-reused".into(),
                        });
                    }
                }
            }
            Err(e) if e.is_retryable() => {
                let delay = e
                    .retry_after()
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| backoff_delay(row.attempt_count as u32));
                let next_at = now_epoch_ms() + delay.as_millis() as i64;
                self.store
                    .reschedule(&row.client_message_id, next_at, Some(e.code()))?;
                tracing::debug!(id = %row.client_message_id, ?delay, error = %e, "outbox: rescheduled");
                let _ = self.events.send(OutboxEvent::Rescheduled {
                    client_message_id: row.client_message_id,
                    next_at,
                    attempt: row.attempt_count + 1,
                    code: e.code().into(),
                });
            }
            Err(e) => {
                tracing::warn!(id = %row.client_message_id, error = %e, "outbox: rejected");
                self.store.mark_rejected(&row.client_message_id, e.code())?;
                let _ = self.events.send(OutboxEvent::Rejected {
                    client_message_id: row.client_message_id,
                    code: e.code().into(),
                });
            }
        }
        Ok(Step::Processed)
    }
}
