//! Local persistence: entities for the five Room tables, the `LocalStore`
//! façade, and a SQLite implementation ([`sqlite::SqliteStore`]).

pub mod sqlite;

use crate::error::Result;
use crate::models::{now_epoch_ms, GroupDto, GroupKind, MemberDto, MessageDto, Permission, Role};
use crate::reconcile::ReconcilePlan;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio::sync::broadcast;

pub use sqlite::SqliteStore;

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// Row of the `groups` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupEntity {
    pub id: String,
    pub name: String,
    pub category: String,
    pub kind: GroupKind,
    pub who_can_post: Permission,
    pub who_can_add_members: Permission,
    pub created_by: Option<String>,
    /// Epoch ms.
    pub created_at: i64,
    pub my_role: Role,
    pub member_count: i64,
    pub muted: bool,
    pub last_activity_at: Option<i64>,
    pub last_message_preview: Option<String>,
    pub is_deleted: bool,
    pub last_read_seq: i64,
}

impl GroupEntity {
    pub fn is_system(&self) -> bool {
        self.kind == GroupKind::System
    }
    /// `CanPostUseCase`.
    pub fn can_post(&self) -> bool {
        !self.is_system() && self.who_can_post.allows(self.my_role)
    }
    /// `CanAddMembersUseCase`.
    pub fn can_add_members(&self) -> bool {
        self.who_can_add_members.allows(self.my_role)
    }
    pub fn is_admin(&self) -> bool {
        self.my_role == Role::Admin
    }
}

impl From<GroupDto> for GroupEntity {
    fn from(g: GroupDto) -> Self {
        GroupEntity {
            id: g.id,
            name: g.name,
            category: g.category,
            kind: g.kind,
            who_can_post: g.who_can_post,
            who_can_add_members: g.who_can_add_members,
            created_by: g.created_by,
            created_at: g.created_at.unwrap_or_else(now_epoch_ms),
            my_role: g.my_role,
            member_count: g.member_count,
            muted: g.muted,
            last_activity_at: g.last_activity_at,
            last_message_preview: g.last_message_preview,
            is_deleted: false,
            last_read_seq: g.last_read_seq.unwrap_or(0),
        }
    }
}

/// `observeGroupsWithUnread` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupWithUnread {
    pub group: GroupEntity,
    pub unread_count: i64,
    /// Highest stored seq, if any messages are stored.
    pub max_seq: Option<i64>,
}

/// Row of the `messages` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEntity {
    pub id: String,
    pub group_id: String,
    pub seq: i64,
    pub sender_id: String,
    pub body: String,
    /// Set if this user sent it.
    pub client_message_id: Option<String>,
    pub created_at: i64,
}

impl MessageEntity {
    /// Convert a DTO; `fallback_group_id` is used when the DTO omits `groupId`
    /// (WS `messages` frames carry it on the frame instead).
    pub fn from_dto(m: MessageDto, fallback_group_id: &str) -> Self {
        MessageEntity {
            id: m.id,
            group_id: m.group_id.unwrap_or_else(|| fallback_group_id.to_owned()),
            seq: m.seq,
            sender_id: m.sender_id,
            body: m.body,
            client_message_id: m.client_message_id,
            created_at: m.created_at.unwrap_or_else(now_epoch_ms),
        }
    }
}

/// Row of the `members` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberEntity {
    pub group_id: String,
    pub user_id: String,
    pub display_name: String,
    pub phone_e164: Option<String>,
    pub role: Role,
    pub joined_at: i64,
}

impl MemberEntity {
    pub fn from_dto(m: MemberDto, group_id: &str) -> Self {
        MemberEntity {
            group_id: group_id.to_owned(),
            user_id: m.user_id,
            display_name: m.display_name,
            phone_e164: m.phone_e164,
            role: m.role,
            joined_at: m.joined_at.unwrap_or_else(now_epoch_ms),
        }
    }
}

/// `outbox.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutboxState {
    Pending,
    InFlight,
    Confirmed,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::Pending => "PENDING",
            OutboxState::InFlight => "IN_FLIGHT",
            OutboxState::Confirmed => "CONFIRMED",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(OutboxState::Pending),
            "IN_FLIGHT" => Some(OutboxState::InFlight),
            "CONFIRMED" => Some(OutboxState::Confirmed),
            _ => None,
        }
    }
}

/// UI state of an outgoing message (`OutgoingState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutgoingState {
    /// In outbox, not yet acknowledged by server.
    Pending,
    /// Server confirmed (echo received or POST succeeded).
    Sent,
    /// Error code set on outbox row.
    Failed,
}

/// Row of the `outbox` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntity {
    /// UUID primary key.
    pub client_message_id: String,
    pub group_id: String,
    pub body: String,
    pub state: OutboxState,
    pub error_code: Option<String>,
    pub attempt_count: i64,
    pub next_attempt_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub created_at: i64,
    pub confirmed_message_id: Option<String>,
    pub confirmed_seq: Option<i64>,
}

impl OutboxEntity {
    /// New pending entry with a fresh UUID.
    pub fn new(group_id: &str, body: &str) -> Self {
        OutboxEntity {
            client_message_id: uuid::Uuid::new_v4().to_string(),
            group_id: group_id.to_owned(),
            body: body.to_owned(),
            state: OutboxState::Pending,
            error_code: None,
            attempt_count: 0,
            next_attempt_at: None,
            last_attempt_at: None,
            created_at: now_epoch_ms(),
            confirmed_message_id: None,
            confirmed_seq: None,
        }
    }

    pub fn outgoing_state(&self) -> OutgoingState {
        match self.state {
            OutboxState::Confirmed => OutgoingState::Sent,
            _ if self.error_code.is_some() && self.next_attempt_at.is_none() => {
                OutgoingState::Failed
            }
            _ => OutgoingState::Pending,
        }
    }
}

/// Row of the `local_command_replies` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCommandReplyEntity {
    /// Autoincrement id; `0`/`None` before insert.
    pub id: Option<i64>,
    pub group_id: String,
    pub command_name: String,
    pub ok: bool,
    pub code: String,
    pub params_json: Option<String>,
    pub text: String,
    pub created_at: i64,
}

/// Which part of the store changed (the analogue of Room `Flow` invalidation).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoreChange {
    Groups,
    Messages {
        group_id: String,
    },
    Members {
        group_id: String,
    },
    Outbox {
        group_id: String,
    },
    CommandReplies {
        group_id: String,
    },
    /// Every table was cleared (session wipe).
    Cleared,
}

/// Result of applying an incoming batch inside one transaction
/// (`DeliveryTransactions.storeIncomingBatch`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StoreBatchOutcome {
    pub plan: ReconcilePlan,
    /// Rows actually written to `messages`.
    pub inserted: usize,
}

// ---------------------------------------------------------------------------
// LocalStore façade
// ---------------------------------------------------------------------------

/// Façade over all DAOs. All methods are synchronous and cheap (SQLite on a
/// local file); wrap calls in `spawn_blocking` if you are on a hot async path.
pub trait LocalStore: Send + Sync {
    /// Subscribe to change notifications.
    fn subscribe(&self) -> broadcast::Receiver<StoreChange>;

    // ---- groups ----
    fn upsert_group(&self, g: &GroupEntity) -> Result<()>;
    fn get_group(&self, id: &str) -> Result<Option<GroupEntity>>;
    /// All non-deleted groups, most recent activity first.
    fn groups(&self) -> Result<Vec<GroupEntity>>;
    /// `observeGroupsWithUnread(selfId)`: non-deleted groups with unread counts
    /// (messages with `seq > lastReadSeq` not sent by `self_user_id`).
    fn groups_with_unread(&self, self_user_id: &str) -> Result<Vec<GroupWithUnread>>;
    /// `UPDATE SET lastReadSeq = MAX(lastReadSeq, ?)`.
    fn mark_read(&self, group_id: &str, seq: i64) -> Result<()>;
    fn mark_group_deleted(&self, id: &str) -> Result<()>;
    fn mark_all_groups_deleted(&self) -> Result<()>;
    fn mark_groups_deleted_except(&self, ids: &[String]) -> Result<()>;
    fn set_muted(&self, id: &str, muted: bool) -> Result<()>;
    fn purge_deleted_groups(&self) -> Result<usize>;
    /// Upsert every group and mark deleted any not in the list.
    fn reconcile_groups(&self, groups: &[GroupEntity]) -> Result<()>;
    /// Apply a `group-updated` event.
    fn apply_group_updated(
        &self,
        id: &str,
        name: Option<&str>,
        who_can_post: Option<&Permission>,
        who_can_add_members: Option<&Permission>,
    ) -> Result<()>;
    fn set_group_member_count(&self, id: &str, delta: i64) -> Result<()>;
    fn set_my_role(&self, id: &str, role: Role) -> Result<()>;

    // ---- messages ----
    /// `INSERT OR IGNORE`; returns the number of rows inserted.
    fn insert_messages(&self, msgs: &[MessageEntity]) -> Result<usize>;
    /// Latest `limit` messages in ascending `seq` order.
    fn thread(&self, group_id: &str, limit: u32) -> Result<Vec<MessageEntity>>;
    fn max_seq(&self, group_id: &str) -> Result<Option<i64>>;
    fn min_seq(&self, group_id: &str) -> Result<Option<i64>>;
    fn latest_message(&self, group_id: &str) -> Result<Option<MessageEntity>>;
    fn count_between(&self, group_id: &str, from: i64, to: i64) -> Result<i64>;
    fn delete_messages_for_group(&self, group_id: &str) -> Result<usize>;
    fn message_ids_exist(&self, group_id: &str, ids: &[String]) -> Result<HashSet<String>>;
    fn seqs_exist(&self, group_id: &str, seqs: &[i64]) -> Result<HashSet<i64>>;
    /// Reconcile + insert + confirm echoes + refresh group preview, atomically.
    fn store_incoming_batch(
        &self,
        group_id: &str,
        batch: Vec<MessageEntity>,
    ) -> Result<StoreBatchOutcome>;

    // ---- members ----
    fn upsert_member(&self, m: &MemberEntity) -> Result<()>;
    fn members(&self, group_id: &str) -> Result<Vec<MemberEntity>>;
    fn delete_member(&self, group_id: &str, user_id: &str) -> Result<()>;
    fn delete_all_members(&self, group_id: &str) -> Result<()>;
    fn set_role(&self, group_id: &str, user_id: &str, role: Role) -> Result<()>;
    /// `deleteAll + bulk upsert` in one transaction.
    fn replace_members(&self, group_id: &str, members: &[MemberEntity]) -> Result<()>;

    // ---- outbox ----
    fn insert_outbox(&self, e: &OutboxEntity) -> Result<()>;
    /// `COUNT WHERE state IN ('PENDING','IN_FLIGHT')`.
    fn pending_count(&self) -> Result<i64>;
    fn unconfirmed_ids(&self, group_id: &str) -> Result<HashSet<String>>;
    fn get_outbox(&self, client_message_id: &str) -> Result<Option<OutboxEntity>>;
    /// Non-confirmed outbox rows for a group, oldest first.
    fn pending_outbox(&self, group_id: &str) -> Result<Vec<OutboxEntity>>;
    fn delete_outbox(&self, client_message_id: &str) -> Result<()>;
    fn purge_old_confirmed(&self, before: i64) -> Result<usize>;
    /// Next row to dispatch: a due `PENDING` row, or an `IN_FLIGHT` row whose
    /// last attempt is older than `stale_before`.
    fn next_dispatchable(&self, now: i64, stale_before: i64) -> Result<Option<OutboxEntity>>;
    /// Earliest future `nextAttemptAt` among `PENDING` rows, if any.
    fn earliest_next_attempt(&self, now: i64) -> Result<Option<i64>>;
    fn mark_in_flight(&self, client_message_id: &str, at: i64) -> Result<()>;
    /// Permanent rejection: back to `PENDING` with `errorCode` set and no
    /// scheduled retry (shown as `Failed` until [`LocalStore::retry_outbox`]).
    fn mark_rejected(&self, client_message_id: &str, error_code: &str) -> Result<()>;
    /// Transient failure: back to `PENDING`, `attemptCount + 1`, retry at `next_at`.
    fn reschedule(
        &self,
        client_message_id: &str,
        next_at: i64,
        error_code: Option<&str>,
    ) -> Result<()>;
    /// Clear the error and make the row dispatchable now.
    fn retry_outbox(&self, client_message_id: &str) -> Result<()>;
    /// Move an outbox row to `CONFIRMED` with the server's id/seq.
    fn confirm_sent(&self, client_message_id: &str, server_id: &str, seq: i64) -> Result<()>;

    // ---- command replies ----
    fn insert_command_reply(&self, e: &LocalCommandReplyEntity) -> Result<i64>;
    fn command_replies(&self, group_id: &str) -> Result<Vec<LocalCommandReplyEntity>>;
    fn clear_command_replies(&self, group_id: &str) -> Result<usize>;

    // ---- session wipe ----
    /// Delete every row from every table.
    fn clear_all(&self) -> Result<()>;
}
