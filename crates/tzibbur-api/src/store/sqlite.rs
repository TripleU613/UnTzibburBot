//! SQLite implementation of [`LocalStore`], reproducing the Room schema
//! (version 2) of the Android client exactly.

use super::*;
use crate::constants::DB_SCHEMA_VERSION;
use crate::error::{AppError, Result};
use crate::reconcile::{reconcile, ReconcileState};
use parking_lot::Mutex;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, Transaction};
use std::path::Path;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS `groups` (
  `id`                 TEXT    NOT NULL,
  `name`               TEXT    NOT NULL,
  `category`           TEXT    NOT NULL,
  `kind`               TEXT    NOT NULL,
  `whoCanPost`         TEXT    NOT NULL,
  `whoCanAddMembers`   TEXT    NOT NULL,
  `createdBy`          TEXT,
  `createdAt`          INTEGER NOT NULL,
  `myRole`             TEXT    NOT NULL,
  `memberCount`        INTEGER NOT NULL,
  `muted`              INTEGER NOT NULL,
  `lastActivityAt`     INTEGER,
  `lastMessagePreview` TEXT,
  `isDeleted`          INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(`id`)
);
CREATE TABLE IF NOT EXISTS `messages` (
  `id`              TEXT    NOT NULL,
  `groupId`         TEXT    NOT NULL,
  `seq`             INTEGER NOT NULL,
  `senderId`        TEXT    NOT NULL,
  `body`            TEXT    NOT NULL,
  `clientMessageId` TEXT,
  `createdAt`       INTEGER NOT NULL,
  PRIMARY KEY(`id`)
);
CREATE UNIQUE INDEX IF NOT EXISTS `index_messages_groupId_seq` ON `messages` (`groupId` ASC, `seq` DESC);
CREATE TABLE IF NOT EXISTS `members` (
  `groupId`     TEXT    NOT NULL,
  `userId`      TEXT    NOT NULL,
  `displayName` TEXT    NOT NULL,
  `phoneE164`   TEXT,
  `role`        TEXT    NOT NULL,
  `joinedAt`    INTEGER NOT NULL,
  PRIMARY KEY(`groupId`, `userId`),
  FOREIGN KEY(`groupId`) REFERENCES `groups`(`id`) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS `index_members_groupId` ON `members` (`groupId`);
CREATE TABLE IF NOT EXISTS `outbox` (
  `clientMessageId`    TEXT    NOT NULL,
  `groupId`            TEXT    NOT NULL,
  `body`               TEXT    NOT NULL,
  `state`              TEXT    NOT NULL,
  `errorCode`          TEXT,
  `attemptCount`       INTEGER NOT NULL,
  `nextAttemptAt`      INTEGER,
  `lastAttemptAt`      INTEGER,
  `createdAt`          INTEGER NOT NULL,
  `confirmedMessageId` TEXT,
  `confirmedSeq`       INTEGER,
  PRIMARY KEY(`clientMessageId`)
);
CREATE INDEX IF NOT EXISTS `index_outbox_groupId` ON `outbox` (`groupId`);
CREATE INDEX IF NOT EXISTS `index_outbox_state`   ON `outbox` (`state`);
CREATE TABLE IF NOT EXISTS `local_command_replies` (
  `id`          INTEGER PRIMARY KEY AUTOINCREMENT,
  `groupId`     TEXT    NOT NULL,
  `commandName` TEXT    NOT NULL,
  `ok`          INTEGER NOT NULL,
  `code`        TEXT    NOT NULL,
  `paramsJson`  TEXT,
  `text`        TEXT    NOT NULL,
  `createdAt`   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS `index_local_command_replies_groupId` ON `local_command_replies` (`groupId`);
"#;

/// Migration 1→2 from the Android client.
const MIGRATION_1_2: &str = "ALTER TABLE groups ADD COLUMN lastReadSeq INTEGER NOT NULL DEFAULT 0";

const GROUP_COLS: &str = "id, name, category, kind, whoCanPost, whoCanAddMembers, createdBy, createdAt, myRole, memberCount, muted, lastActivityAt, lastMessagePreview, isDeleted, lastReadSeq";
const MESSAGE_COLS: &str = "id, groupId, seq, senderId, body, clientMessageId, createdAt";
const MEMBER_COLS: &str = "groupId, userId, displayName, phoneE164, role, joinedAt";
const OUTBOX_COLS: &str = "clientMessageId, groupId, body, state, errorCode, attemptCount, nextAttemptAt, lastAttemptAt, createdAt, confirmedMessageId, confirmedSeq";
const REPLY_COLS: &str = "id, groupId, commandName, ok, code, paramsJson, text, createdAt";

/// SQLite-backed [`LocalStore`].
pub struct SqliteStore {
    conn: Mutex<Connection>,
    changes: broadcast::Sender<StoreChange>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteStore")
    }
}

impl SqliteStore {
    /// Open (or create) the database file and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// In-memory database (tests, ephemeral bridges).
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    pub fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
        )?;
        Self::migrate(&conn)?;
        let (changes, _) = broadcast::channel(1024);
        Ok(Self {
            conn: Mutex::new(conn),
            changes,
        })
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA_V1)?;
            let has_col = conn
                .prepare("SELECT 1 FROM pragma_table_info('groups') WHERE name = 'lastReadSeq'")?
                .exists([])?;
            if !has_col {
                conn.execute_batch(MIGRATION_1_2)?;
            }
        } else if version == 1 {
            conn.execute_batch(MIGRATION_1_2)?;
        } else if version > DB_SCHEMA_VERSION {
            return Err(AppError::Store(format!(
                "database schema version {version} is newer than supported {DB_SCHEMA_VERSION}"
            )));
        }
        conn.pragma_update(None, "user_version", DB_SCHEMA_VERSION)?;
        Self::repair(conn)?;
        Ok(())
    }

    /// Fix inconsistent outbox rows left by older versions or crashes.
    fn repair(conn: &Connection) -> Result<()> {
        // Delivered (server id/seq known) but state was clobbered by a later rejection.
        let fixed = conn.execute(
            "UPDATE outbox SET state = 'CONFIRMED', errorCode = NULL, nextAttemptAt = NULL
             WHERE confirmedSeq IS NOT NULL AND state != 'CONFIRMED'",
            [],
        )?;
        // "Failed" only because we could not read the reply: verify them, don't leave them dead.
        let sched = conn.execute(
            "UPDATE outbox SET nextAttemptAt = ?1
             WHERE state = 'PENDING' AND nextAttemptAt IS NULL AND errorCode IN ('json', 'client-message-id-reused')",
            params![now_epoch_ms()],
        )?;
        if fixed + sched > 0 {
            tracing::info!(fixed, scheduled = sched, "outbox repaired");
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self
            .conn
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn notify(&self, c: StoreChange) {
        let _ = self.changes.send(c);
    }

    fn with_tx<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

// ---- row mappers -------------------------------------------------------------

fn group_from_row(r: &Row<'_>) -> rusqlite::Result<GroupEntity> {
    Ok(GroupEntity {
        id: r.get(0)?,
        name: r.get(1)?,
        category: r.get(2)?,
        kind: GroupKind::parse(&r.get::<_, String>(3)?),
        who_can_post: Permission(r.get(4)?),
        who_can_add_members: Permission(r.get(5)?),
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        my_role: Role::parse(&r.get::<_, String>(8)?).unwrap_or_default(),
        member_count: r.get(9)?,
        muted: r.get::<_, i64>(10)? != 0,
        last_activity_at: r.get(11)?,
        last_message_preview: r.get(12)?,
        is_deleted: r.get::<_, i64>(13)? != 0,
        last_read_seq: r.get(14)?,
    })
}

fn message_from_row(r: &Row<'_>) -> rusqlite::Result<MessageEntity> {
    Ok(MessageEntity {
        id: r.get(0)?,
        group_id: r.get(1)?,
        seq: r.get(2)?,
        sender_id: r.get(3)?,
        body: r.get(4)?,
        client_message_id: r.get(5)?,
        created_at: r.get(6)?,
    })
}

fn member_from_row(r: &Row<'_>) -> rusqlite::Result<MemberEntity> {
    Ok(MemberEntity {
        group_id: r.get(0)?,
        user_id: r.get(1)?,
        display_name: r.get(2)?,
        phone_e164: r.get(3)?,
        role: Role::parse(&r.get::<_, String>(4)?).unwrap_or_default(),
        joined_at: r.get(5)?,
    })
}

fn outbox_from_row(r: &Row<'_>) -> rusqlite::Result<OutboxEntity> {
    Ok(OutboxEntity {
        client_message_id: r.get(0)?,
        group_id: r.get(1)?,
        body: r.get(2)?,
        state: OutboxState::parse(&r.get::<_, String>(3)?).unwrap_or(OutboxState::Pending),
        error_code: r.get(4)?,
        attempt_count: r.get(5)?,
        next_attempt_at: r.get(6)?,
        last_attempt_at: r.get(7)?,
        created_at: r.get(8)?,
        confirmed_message_id: r.get(9)?,
        confirmed_seq: r.get(10)?,
    })
}

fn reply_from_row(r: &Row<'_>) -> rusqlite::Result<LocalCommandReplyEntity> {
    Ok(LocalCommandReplyEntity {
        id: r.get(0)?,
        group_id: r.get(1)?,
        command_name: r.get(2)?,
        ok: r.get::<_, i64>(3)? != 0,
        code: r.get(4)?,
        params_json: r.get(5)?,
        text: r.get(6)?,
        created_at: r.get(7)?,
    })
}

fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

// ---- transactional helpers (shared by several trait methods) -----------------

fn tx_upsert_group(tx: &Transaction<'_>, g: &GroupEntity) -> Result<()> {
    // Room used INSERT OR IGNORE plus targeted UPDATEs; we merge server fields while
    // preserving the local-only columns (muted, lastReadSeq) and un-deleting.
    tx.execute(
        &format!(
            "INSERT INTO groups ({GROUP_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0,?14)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name, category = excluded.category, kind = excluded.kind,
               whoCanPost = excluded.whoCanPost, whoCanAddMembers = excluded.whoCanAddMembers,
               createdBy = COALESCE(excluded.createdBy, groups.createdBy),
               createdAt = excluded.createdAt, myRole = excluded.myRole,
               memberCount = excluded.memberCount,
               lastActivityAt = COALESCE(excluded.lastActivityAt, groups.lastActivityAt),
               lastMessagePreview = COALESCE(excluded.lastMessagePreview, groups.lastMessagePreview),
               isDeleted = 0,
               lastReadSeq = MAX(groups.lastReadSeq, excluded.lastReadSeq)"
        ),
        params![
            g.id,
            g.name,
            g.category,
            g.kind.as_str(),
            g.who_can_post.0,
            g.who_can_add_members.0,
            g.created_by,
            g.created_at,
            g.my_role.as_str(),
            g.member_count,
            g.muted as i64,
            g.last_activity_at,
            g.last_message_preview,
            g.last_read_seq,
        ],
    )?;
    Ok(())
}

fn tx_upsert_member(tx: &Transaction<'_>, m: &MemberEntity) -> Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO members ({MEMBER_COLS}) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(groupId, userId) DO UPDATE SET
               displayName = excluded.displayName, phoneE164 = excluded.phoneE164,
               role = excluded.role, joinedAt = excluded.joinedAt"
        ),
        params![
            m.group_id,
            m.user_id,
            m.display_name,
            m.phone_e164,
            m.role.as_str(),
            m.joined_at
        ],
    )?;
    Ok(())
}

fn tx_confirm_sent(
    tx: &Transaction<'_>,
    client_message_id: &str,
    server_id: &str,
    seq: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE outbox SET state = 'CONFIRMED', errorCode = NULL, nextAttemptAt = NULL,
                confirmedMessageId = ?2, confirmedSeq = ?3
         WHERE clientMessageId = ?1",
        params![client_message_id, server_id, seq],
    )?;
    Ok(())
}

fn tx_refresh_group_preview(tx: &Transaction<'_>, group_id: &str) -> Result<()> {
    tx.execute(
        "UPDATE groups SET
            lastMessagePreview = NULLIF((SELECT body FROM messages WHERE groupId = ?1 ORDER BY seq DESC LIMIT 1), ''),
            lastActivityAt = MAX(COALESCE(lastActivityAt, 0),
                                 COALESCE((SELECT createdAt FROM messages WHERE groupId = ?1 ORDER BY seq DESC LIMIT 1), 0))
         WHERE id = ?1",
        params![group_id],
    )?;
    Ok(())
}

impl LocalStore for SqliteStore {
    fn subscribe(&self) -> broadcast::Receiver<StoreChange> {
        self.changes.subscribe()
    }

    // ---- groups ----

    fn upsert_group(&self, g: &GroupEntity) -> Result<()> {
        self.with_tx(|tx| tx_upsert_group(tx, g))?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn get_group(&self, id: &str) -> Result<Option<GroupEntity>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!("SELECT {GROUP_COLS} FROM groups WHERE id = ?1"),
                params![id],
                group_from_row,
            )
            .optional()?)
    }

    fn groups(&self) -> Result<Vec<GroupEntity>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {GROUP_COLS} FROM groups WHERE isDeleted = 0 ORDER BY COALESCE(lastActivityAt, createdAt) DESC, name ASC"
        ))?;
        let rows = st
            .query_map([], group_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn groups_with_unread(&self, self_user_id: &str) -> Result<Vec<GroupWithUnread>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {}, 
                (SELECT COUNT(*) FROM messages m WHERE m.groupId = g.id AND m.seq > g.lastReadSeq AND m.senderId != ?1) AS unread,
                (SELECT MAX(seq) FROM messages m WHERE m.groupId = g.id) AS maxSeq
             FROM groups g WHERE g.isDeleted = 0
             ORDER BY COALESCE(g.lastActivityAt, g.createdAt) DESC, g.name ASC",
            GROUP_COLS.split(", ").map(|c| format!("g.{c}")).collect::<Vec<_>>().join(", ")
        ))?;
        let rows = st
            .query_map(params![self_user_id], |r| {
                Ok(GroupWithUnread {
                    group: group_from_row(r)?,
                    unread_count: r.get(15)?,
                    max_seq: r.get(16)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn mark_read(&self, group_id: &str, seq: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE groups SET lastReadSeq = MAX(lastReadSeq, ?2) WHERE id = ?1",
            params![group_id, seq],
        )?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn mark_group_deleted(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("UPDATE groups SET isDeleted = 1 WHERE id = ?1", params![id])?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn mark_all_groups_deleted(&self) -> Result<()> {
        self.conn
            .lock()
            .execute("UPDATE groups SET isDeleted = 1", [])?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn mark_groups_deleted_except(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return self.mark_all_groups_deleted();
        }
        self.conn.lock().execute(
            &format!(
                "UPDATE groups SET isDeleted = 1 WHERE id NOT IN ({})",
                placeholders(ids.len())
            ),
            params_from_iter(ids.iter()),
        )?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn set_muted(&self, id: &str, muted: bool) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE groups SET muted = ?2 WHERE id = ?1",
            params![id, muted as i64],
        )?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn purge_deleted_groups(&self) -> Result<usize> {
        let n = self.with_tx(|tx| {
            tx.execute("DELETE FROM messages WHERE groupId IN (SELECT id FROM groups WHERE isDeleted = 1)", [])?;
            tx.execute("DELETE FROM outbox WHERE groupId IN (SELECT id FROM groups WHERE isDeleted = 1)", [])?;
            tx.execute("DELETE FROM local_command_replies WHERE groupId IN (SELECT id FROM groups WHERE isDeleted = 1)", [])?;
            Ok(tx.execute("DELETE FROM groups WHERE isDeleted = 1", [])?)
        })?;
        self.notify(StoreChange::Groups);
        Ok(n)
    }

    fn reconcile_groups(&self, groups: &[GroupEntity]) -> Result<()> {
        self.with_tx(|tx| {
            for g in groups {
                tx_upsert_group(tx, g)?;
            }
            if groups.is_empty() {
                tx.execute("UPDATE groups SET isDeleted = 1", [])?;
            } else {
                let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
                tx.execute(
                    &format!(
                        "UPDATE groups SET isDeleted = 1 WHERE id NOT IN ({})",
                        placeholders(ids.len())
                    ),
                    params_from_iter(ids.iter()),
                )?;
            }
            Ok(())
        })?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn apply_group_updated(
        &self,
        id: &str,
        name: Option<&str>,
        who_can_post: Option<&Permission>,
        who_can_add_members: Option<&Permission>,
    ) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE groups SET
                name = COALESCE(?2, name),
                whoCanPost = COALESCE(?3, whoCanPost),
                whoCanAddMembers = COALESCE(?4, whoCanAddMembers)
             WHERE id = ?1",
            params![
                id,
                name,
                who_can_post.map(|p| p.0.as_str()),
                who_can_add_members.map(|p| p.0.as_str())
            ],
        )?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn set_group_member_count(&self, id: &str, delta: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE groups SET memberCount = MAX(0, memberCount + ?2) WHERE id = ?1",
            params![id, delta],
        )?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    fn set_my_role(&self, id: &str, role: Role) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE groups SET myRole = ?2 WHERE id = ?1",
            params![id, role.as_str()],
        )?;
        self.notify(StoreChange::Groups);
        Ok(())
    }

    // ---- messages ----

    fn insert_messages(&self, msgs: &[MessageEntity]) -> Result<usize> {
        if msgs.is_empty() {
            return Ok(0);
        }
        let mut groups: HashSet<String> = HashSet::new();
        let n = self.with_tx(|tx| {
            let mut st = tx.prepare(&format!(
                "INSERT OR IGNORE INTO messages ({MESSAGE_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7)"
            ))?;
            let mut n = 0;
            for m in msgs {
                n += st.execute(params![
                    m.id,
                    m.group_id,
                    m.seq,
                    m.sender_id,
                    m.body,
                    m.client_message_id,
                    m.created_at
                ])?;
                groups.insert(m.group_id.clone());
            }
            drop(st);
            for g in &groups {
                tx_refresh_group_preview(tx, g)?;
            }
            Ok(n)
        })?;
        for g in groups {
            self.notify(StoreChange::Messages { group_id: g });
        }
        self.notify(StoreChange::Groups);
        Ok(n)
    }

    fn thread(&self, group_id: &str, limit: u32) -> Result<Vec<MessageEntity>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM (SELECT * FROM messages WHERE groupId = ?1 ORDER BY seq DESC LIMIT ?2) ORDER BY seq ASC"
        ))?;
        let rows = st
            .query_map(params![group_id, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn max_seq(&self, group_id: &str) -> Result<Option<i64>> {
        Ok(self.conn.lock().query_row(
            "SELECT MAX(seq) FROM messages WHERE groupId = ?1",
            params![group_id],
            |r| r.get(0),
        )?)
    }

    fn min_seq(&self, group_id: &str) -> Result<Option<i64>> {
        Ok(self.conn.lock().query_row(
            "SELECT MIN(seq) FROM messages WHERE groupId = ?1",
            params![group_id],
            |r| r.get(0),
        )?)
    }

    fn latest_message(&self, group_id: &str) -> Result<Option<MessageEntity>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!("SELECT {MESSAGE_COLS} FROM messages WHERE groupId = ?1 ORDER BY seq DESC LIMIT 1"),
                params![group_id],
                message_from_row,
            )
            .optional()?)
    }

    fn count_between(&self, group_id: &str, from: i64, to: i64) -> Result<i64> {
        Ok(self.conn.lock().query_row(
            "SELECT COUNT(*) FROM messages WHERE groupId = ?1 AND seq BETWEEN ?2 AND ?3",
            params![group_id, from, to],
            |r| r.get(0),
        )?)
    }

    fn delete_messages_for_group(&self, group_id: &str) -> Result<usize> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM messages WHERE groupId = ?1", params![group_id])?;
        self.notify(StoreChange::Messages {
            group_id: group_id.to_owned(),
        });
        Ok(n)
    }

    fn message_ids_exist(&self, group_id: &str, ids: &[String]) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let conn = self.conn.lock();
        for chunk in ids.chunks(500) {
            let mut st = conn.prepare(&format!(
                "SELECT id FROM messages WHERE groupId = ? AND id IN ({})",
                placeholders(chunk.len())
            ))?;
            let p = std::iter::once(group_id.to_owned()).chain(chunk.iter().cloned());
            for r in st.query_map(params_from_iter(p), |r| r.get::<_, String>(0))? {
                out.insert(r?);
            }
        }
        Ok(out)
    }

    fn seqs_exist(&self, group_id: &str, seqs: &[i64]) -> Result<HashSet<i64>> {
        let mut out = HashSet::new();
        if seqs.is_empty() {
            return Ok(out);
        }
        let conn = self.conn.lock();
        for chunk in seqs.chunks(500) {
            let mut st = conn.prepare(&format!(
                "SELECT seq FROM messages WHERE groupId = ? AND seq IN ({})",
                placeholders(chunk.len())
            ))?;
            let p: Vec<rusqlite::types::Value> =
                std::iter::once(rusqlite::types::Value::Text(group_id.to_owned()))
                    .chain(chunk.iter().map(|s| rusqlite::types::Value::Integer(*s)))
                    .collect();
            for r in st.query_map(params_from_iter(p.iter()), |r| r.get::<_, i64>(0))? {
                out.insert(r?);
            }
        }
        Ok(out)
    }

    fn store_incoming_batch(
        &self,
        group_id: &str,
        batch: Vec<MessageEntity>,
    ) -> Result<StoreBatchOutcome> {
        if batch.is_empty() {
            return Ok(StoreBatchOutcome::default());
        }
        let outcome = self.with_tx(|tx| {
            // Gather current state.
            let mut state = ReconcileState::default();
            {
                let ids: Vec<&str> = batch.iter().map(|m| m.id.as_str()).collect();
                let mut st = tx.prepare(&format!(
                    "SELECT id FROM messages WHERE groupId = ? AND id IN ({})",
                    placeholders(ids.len())
                ))?;
                let p = std::iter::once(group_id).chain(ids.iter().copied());
                for r in st.query_map(params_from_iter(p), |r| r.get::<_, String>(0))? {
                    state.existing_ids.insert(r?);
                }
            }
            {
                let seqs: Vec<rusqlite::types::Value> = std::iter::once(rusqlite::types::Value::Text(group_id.to_owned()))
                    .chain(batch.iter().map(|m| rusqlite::types::Value::Integer(m.seq)))
                    .collect();
                let mut st = tx.prepare(&format!(
                    "SELECT seq FROM messages WHERE groupId = ? AND seq IN ({})",
                    placeholders(seqs.len() - 1)
                ))?;
                for r in st.query_map(params_from_iter(seqs.iter()), |r| r.get::<_, i64>(0))? {
                    state.existing_seqs.insert(r?);
                }
            }
            {
                let mut st = tx.prepare("SELECT clientMessageId FROM outbox WHERE groupId = ?1 AND state != 'CONFIRMED'")?;
                for r in st.query_map(params![group_id], |r| r.get::<_, String>(0))? {
                    state.unconfirmed_client_message_ids.insert(r?);
                }
            }

            let plan = reconcile(batch, &state);

            let mut inserted = 0;
            {
                let mut st = tx.prepare(&format!("INSERT OR IGNORE INTO messages ({MESSAGE_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7)"))?;
                for m in &plan.to_insert {
                    inserted += st.execute(params![m.id, m.group_id, m.seq, m.sender_id, m.body, m.client_message_id, m.created_at])?;
                }
            }
            for echo in &plan.echo_confirmations {
                tx_confirm_sent(tx, &echo.client_message_id, &echo.server_id, echo.seq)?;
            }
            if inserted > 0 {
                tx_refresh_group_preview(tx, group_id)?;
            }
            Ok(StoreBatchOutcome { plan, inserted })
        })?;

        if outcome.inserted > 0 {
            self.notify(StoreChange::Messages {
                group_id: group_id.to_owned(),
            });
            self.notify(StoreChange::Groups);
        }
        if !outcome.plan.echo_confirmations.is_empty() {
            self.notify(StoreChange::Outbox {
                group_id: group_id.to_owned(),
            });
        }
        Ok(outcome)
    }

    // ---- members ----

    fn upsert_member(&self, m: &MemberEntity) -> Result<()> {
        self.with_tx(|tx| tx_upsert_member(tx, m))?;
        self.notify(StoreChange::Members {
            group_id: m.group_id.clone(),
        });
        Ok(())
    }

    fn members(&self, group_id: &str) -> Result<Vec<MemberEntity>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {MEMBER_COLS} FROM members WHERE groupId = ?1 ORDER BY joinedAt ASC, userId ASC"
        ))?;
        let rows = st
            .query_map(params![group_id], member_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn delete_member(&self, group_id: &str, user_id: &str) -> Result<()> {
        self.conn.lock().execute(
            "DELETE FROM members WHERE groupId = ?1 AND userId = ?2",
            params![group_id, user_id],
        )?;
        self.notify(StoreChange::Members {
            group_id: group_id.to_owned(),
        });
        Ok(())
    }

    fn delete_all_members(&self, group_id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM members WHERE groupId = ?1", params![group_id])?;
        self.notify(StoreChange::Members {
            group_id: group_id.to_owned(),
        });
        Ok(())
    }

    fn set_role(&self, group_id: &str, user_id: &str, role: Role) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE members SET role = ?3 WHERE groupId = ?1 AND userId = ?2",
            params![group_id, user_id, role.as_str()],
        )?;
        self.notify(StoreChange::Members {
            group_id: group_id.to_owned(),
        });
        Ok(())
    }

    fn replace_members(&self, group_id: &str, members: &[MemberEntity]) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute("DELETE FROM members WHERE groupId = ?1", params![group_id])?;
            for m in members {
                tx_upsert_member(tx, m)?;
            }
            tx.execute(
                "UPDATE groups SET memberCount = ?2 WHERE id = ?1",
                params![group_id, members.len() as i64],
            )?;
            Ok(())
        })?;
        self.notify(StoreChange::Members {
            group_id: group_id.to_owned(),
        });
        self.notify(StoreChange::Groups);
        Ok(())
    }

    // ---- outbox ----

    fn insert_outbox(&self, e: &OutboxEntity) -> Result<()> {
        self.conn.lock().execute(
            &format!("INSERT OR IGNORE INTO outbox ({OUTBOX_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"),
            params![
                e.client_message_id,
                e.group_id,
                e.body,
                e.state.as_str(),
                e.error_code,
                e.attempt_count,
                e.next_attempt_at,
                e.last_attempt_at,
                e.created_at,
                e.confirmed_message_id,
                e.confirmed_seq
            ],
        )?;
        self.notify(StoreChange::Outbox {
            group_id: e.group_id.clone(),
        });
        Ok(())
    }

    fn pending_count(&self) -> Result<i64> {
        Ok(self.conn.lock().query_row(
            "SELECT COUNT(*) FROM outbox WHERE state IN ('PENDING','IN_FLIGHT')",
            [],
            |r| r.get(0),
        )?)
    }

    fn unconfirmed_ids(&self, group_id: &str) -> Result<HashSet<String>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT clientMessageId FROM outbox WHERE groupId = ?1 AND state != 'CONFIRMED'",
        )?;
        let rows = st
            .query_map(params![group_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    fn get_outbox(&self, client_message_id: &str) -> Result<Option<OutboxEntity>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!("SELECT {OUTBOX_COLS} FROM outbox WHERE clientMessageId = ?1"),
                params![client_message_id],
                outbox_from_row,
            )
            .optional()?)
    }

    fn pending_outbox(&self, group_id: &str) -> Result<Vec<OutboxEntity>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox WHERE groupId = ?1 AND state != 'CONFIRMED' ORDER BY createdAt ASC"
        ))?;
        let rows = st
            .query_map(params![group_id], outbox_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn delete_outbox(&self, client_message_id: &str) -> Result<()> {
        let gid: Option<String> = self
            .conn
            .lock()
            .query_row(
                "SELECT groupId FROM outbox WHERE clientMessageId = ?1",
                params![client_message_id],
                |r| r.get(0),
            )
            .optional()?;
        self.conn.lock().execute(
            "DELETE FROM outbox WHERE clientMessageId = ?1",
            params![client_message_id],
        )?;
        if let Some(g) = gid {
            self.notify(StoreChange::Outbox { group_id: g });
        }
        Ok(())
    }

    fn purge_old_confirmed(&self, before: i64) -> Result<usize> {
        Ok(self.conn.lock().execute(
            "DELETE FROM outbox WHERE state = 'CONFIRMED' AND createdAt < ?1",
            params![before],
        )?)
    }

    fn next_dispatchable(&self, now: i64, stale_before: i64) -> Result<Option<OutboxEntity>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!(
                    "SELECT {OUTBOX_COLS} FROM outbox
                     WHERE (state = 'PENDING' AND ((nextAttemptAt IS NOT NULL AND nextAttemptAt <= ?1)
                                                OR (nextAttemptAt IS NULL AND errorCode IS NULL)))
                        OR (state = 'IN_FLIGHT' AND (lastAttemptAt IS NULL OR lastAttemptAt <= ?2))
                     ORDER BY createdAt ASC LIMIT 1"
                ),
                params![now, stale_before],
                outbox_from_row,
            )
            .optional()?)
    }

    fn earliest_next_attempt(&self, now: i64) -> Result<Option<i64>> {
        Ok(self.conn.lock().query_row(
            "SELECT MIN(t) FROM (
                SELECT nextAttemptAt AS t FROM outbox WHERE state = 'PENDING' AND nextAttemptAt > ?1
                UNION ALL
                SELECT lastAttemptAt + ?2 AS t FROM outbox WHERE state = 'IN_FLIGHT' AND lastAttemptAt IS NOT NULL
             )",
            params![now, crate::constants::IN_FLIGHT_STALE.as_millis() as i64],
            |r| r.get(0),
        )?)
    }

    fn mark_in_flight(&self, client_message_id: &str, at: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE outbox SET state = 'IN_FLIGHT', lastAttemptAt = ?2 WHERE clientMessageId = ?1 AND state != 'CONFIRMED'",
            params![client_message_id, at],
        )?;
        Ok(())
    }

    fn mark_rejected(&self, client_message_id: &str, error_code: &str) -> Result<()> {
        let gid = self.outbox_group(client_message_id)?;
        self.conn.lock().execute(
            "UPDATE outbox SET state = 'PENDING', errorCode = ?2, nextAttemptAt = NULL, attemptCount = attemptCount + 1
             WHERE clientMessageId = ?1 AND state != 'CONFIRMED'",
            params![client_message_id, error_code],
        )?;
        if let Some(g) = gid {
            self.notify(StoreChange::Outbox { group_id: g });
        }
        Ok(())
    }

    fn reschedule(
        &self,
        client_message_id: &str,
        next_at: i64,
        error_code: Option<&str>,
    ) -> Result<()> {
        let gid = self.outbox_group(client_message_id)?;
        self.conn.lock().execute(
            "UPDATE outbox SET state = 'PENDING', nextAttemptAt = ?2, errorCode = ?3, attemptCount = attemptCount + 1
             WHERE clientMessageId = ?1 AND state != 'CONFIRMED'",
            params![client_message_id, next_at, error_code],
        )?;
        if let Some(g) = gid {
            self.notify(StoreChange::Outbox { group_id: g });
        }
        Ok(())
    }

    fn failed_outbox(&self, group_id: &str) -> Result<Vec<OutboxEntity>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox WHERE groupId = ?1 AND state = 'PENDING' AND errorCode IS NOT NULL AND nextAttemptAt IS NULL ORDER BY createdAt ASC"
        ))?;
        let rows = st
            .query_map(params![group_id], outbox_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn retry_outbox(&self, client_message_id: &str) -> Result<()> {
        let gid = self.outbox_group(client_message_id)?;
        self.conn.lock().execute(
            "UPDATE outbox SET state = 'PENDING', errorCode = NULL, nextAttemptAt = ?2 WHERE clientMessageId = ?1 AND state != 'CONFIRMED'",
            params![client_message_id, now_epoch_ms()],
        )?;
        if let Some(g) = gid {
            self.notify(StoreChange::Outbox { group_id: g });
        }
        Ok(())
    }

    fn confirm_sent(&self, client_message_id: &str, server_id: &str, seq: i64) -> Result<()> {
        let gid = self.outbox_group(client_message_id)?;
        self.with_tx(|tx| tx_confirm_sent(tx, client_message_id, server_id, seq))?;
        if let Some(g) = gid {
            self.notify(StoreChange::Outbox { group_id: g });
        }
        Ok(())
    }

    // ---- command replies ----

    fn insert_command_reply(&self, e: &LocalCommandReplyEntity) -> Result<i64> {
        let id = {
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO local_command_replies (groupId, commandName, ok, code, paramsJson, text, createdAt)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![e.group_id, e.command_name, e.ok as i64, e.code, e.params_json, e.text, e.created_at],
            )?;
            conn.last_insert_rowid()
        };
        self.notify(StoreChange::CommandReplies {
            group_id: e.group_id.clone(),
        });
        Ok(id)
    }

    fn command_replies(&self, group_id: &str) -> Result<Vec<LocalCommandReplyEntity>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!(
            "SELECT {REPLY_COLS} FROM local_command_replies WHERE groupId = ?1 ORDER BY createdAt ASC, id ASC"
        ))?;
        let rows = st
            .query_map(params![group_id], reply_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn clear_command_replies(&self, group_id: &str) -> Result<usize> {
        let n = self.conn.lock().execute(
            "DELETE FROM local_command_replies WHERE groupId = ?1",
            params![group_id],
        )?;
        self.notify(StoreChange::CommandReplies {
            group_id: group_id.to_owned(),
        });
        Ok(n)
    }

    // ---- privacy ----

    fn redact_messages(&self, group_id: &str, up_to_seq: i64) -> Result<usize> {
        let n = self.with_tx(|tx| {
            let n = tx.execute(
                "UPDATE messages SET body = '' WHERE groupId = ?1 AND seq <= ?2 AND body != ''",
                params![group_id, up_to_seq],
            )?;
            tx.execute(
                "UPDATE groups SET lastMessagePreview = NULL WHERE id = ?1",
                params![group_id],
            )?;
            Ok(n)
        })?;
        Ok(n)
    }

    fn redact_confirmed_outbox(&self) -> Result<usize> {
        Ok(self.conn.lock().execute(
            "UPDATE outbox SET body = '' WHERE state = 'CONFIRMED' AND body != ''",
            [],
        )?)
    }

    // ---- wipe ----

    fn clear_all(&self) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute_batch(
                "DELETE FROM local_command_replies; DELETE FROM outbox; DELETE FROM members;
                 DELETE FROM messages; DELETE FROM groups;",
            )?;
            Ok(())
        })?;
        self.notify(StoreChange::Cleared);
        Ok(())
    }
}

impl SqliteStore {
    fn outbox_group(&self, client_message_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT groupId FROM outbox WHERE clientMessageId = ?1",
                params![client_message_id],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str) -> GroupEntity {
        GroupEntity {
            id: id.into(),
            name: "G".into(),
            category: "c".into(),
            kind: GroupKind::Standard,
            who_can_post: Permission::everyone(),
            who_can_add_members: Permission::admins(),
            created_by: None,
            created_at: 1,
            my_role: Role::Member,
            member_count: 2,
            muted: false,
            last_activity_at: None,
            last_message_preview: None,
            is_deleted: false,
            last_read_seq: 0,
        }
    }

    fn msg(id: &str, gid: &str, seq: i64, sender: &str, cmid: Option<&str>) -> MessageEntity {
        MessageEntity {
            id: id.into(),
            group_id: gid.into(),
            seq,
            sender_id: sender.into(),
            body: format!("body {seq}"),
            client_message_id: cmid.map(str::to_owned),
            created_at: seq * 1000,
        }
    }

    #[test]
    fn schema_and_migration() {
        let s = SqliteStore::in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), 2);
        // Simulate a v1 DB and re-open.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let s2 = SqliteStore::from_connection(conn).unwrap();
        assert_eq!(s2.schema_version().unwrap(), 2);
        s2.upsert_group(&group("g")).unwrap();
        assert_eq!(s2.get_group("g").unwrap().unwrap().last_read_seq, 0);
    }

    #[test]
    fn unread_and_mark_read() {
        let s = SqliteStore::in_memory().unwrap();
        s.upsert_group(&group("g")).unwrap();
        s.insert_messages(&[
            msg("a", "g", 1, "other", None),
            msg("b", "g", 2, "me", None),
            msg("c", "g", 3, "other", None),
        ])
        .unwrap();
        let rows = s.groups_with_unread("me").unwrap();
        assert_eq!(rows[0].unread_count, 2);
        assert_eq!(rows[0].max_seq, Some(3));
        assert_eq!(
            rows[0].group.last_message_preview.as_deref(),
            Some("body 3")
        );
        s.mark_read("g", 3).unwrap();
        s.mark_read("g", 1).unwrap(); // MAX() keeps 3
        assert_eq!(s.groups_with_unread("me").unwrap()[0].unread_count, 0);
        assert_eq!(s.get_group("g").unwrap().unwrap().last_read_seq, 3);
    }

    #[test]
    fn outbox_lifecycle_and_echo() {
        let s = SqliteStore::in_memory().unwrap();
        s.upsert_group(&group("g")).unwrap();
        let e = OutboxEntity::new("g", "hello");
        s.insert_outbox(&e).unwrap();
        assert_eq!(s.pending_count().unwrap(), 1);
        let next = s.next_dispatchable(now_epoch_ms(), 0).unwrap().unwrap();
        assert_eq!(next.client_message_id, e.client_message_id);
        s.mark_in_flight(&e.client_message_id, now_epoch_ms())
            .unwrap();
        // Not stale yet → nothing dispatchable.
        assert!(s
            .next_dispatchable(now_epoch_ms(), now_epoch_ms() - 60_000)
            .unwrap()
            .is_none());
        // Echo arrives over WS.
        let out = s
            .store_incoming_batch(
                "g",
                vec![msg("srv1", "g", 10, "me", Some(&e.client_message_id))],
            )
            .unwrap();
        assert_eq!(out.inserted, 1);
        assert_eq!(out.plan.echo_confirmations.len(), 1);
        assert!(out.plan.new_message_ids.is_empty());
        let row = s.get_outbox(&e.client_message_id).unwrap().unwrap();
        assert_eq!(row.state, OutboxState::Confirmed);
        assert_eq!(row.confirmed_seq, Some(10));
        assert_eq!(row.outgoing_state(), OutgoingState::Sent);
        assert_eq!(s.pending_count().unwrap(), 0);
    }

    #[test]
    fn confirm_wins_over_late_rejection() {
        let s = SqliteStore::in_memory().unwrap();
        s.upsert_group(&group("g")).unwrap();
        let e = OutboxEntity::new("g", "x");
        s.insert_outbox(&e).unwrap();
        s.mark_in_flight(&e.client_message_id, 1).unwrap();
        s.confirm_sent(&e.client_message_id, "srv", 7).unwrap();
        s.mark_rejected(&e.client_message_id, "json").unwrap();
        s.reschedule(&e.client_message_id, 5, Some("x")).unwrap();
        let r = s.get_outbox(&e.client_message_id).unwrap().unwrap();
        assert_eq!(r.state, OutboxState::Confirmed);
        assert_eq!(r.confirmed_seq, Some(7));
        assert!(r.error_code.is_none());
    }

    #[test]
    fn rejected_is_failed_until_retry() {
        let s = SqliteStore::in_memory().unwrap();
        s.upsert_group(&group("g")).unwrap();
        let e = OutboxEntity::new("g", "x");
        s.insert_outbox(&e).unwrap();
        s.mark_rejected(&e.client_message_id, "invalid-message")
            .unwrap();
        assert_eq!(
            s.get_outbox(&e.client_message_id)
                .unwrap()
                .unwrap()
                .outgoing_state(),
            OutgoingState::Failed
        );
        assert!(s.next_dispatchable(now_epoch_ms(), 0).unwrap().is_none());
        s.retry_outbox(&e.client_message_id).unwrap();
        assert!(s
            .next_dispatchable(now_epoch_ms() + 1, 0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn redaction_keeps_ids_drops_text() {
        let s = SqliteStore::in_memory().unwrap();
        s.upsert_group(&group("g")).unwrap();
        s.insert_messages(&[msg("a", "g", 1, "u", None), msg("b", "g", 2, "u", None)])
            .unwrap();
        assert_eq!(s.redact_messages("g", 1).unwrap(), 1);
        let t = s.thread("g", 10).unwrap();
        assert_eq!((t[0].body.as_str(), t[1].body.as_str()), ("", "body 2"));
        assert!(s
            .get_group("g")
            .unwrap()
            .unwrap()
            .last_message_preview
            .is_none());
        // Dedup still works on ids/seqs after redaction.
        let out = s
            .store_incoming_batch("g", vec![msg("a", "g", 1, "u", None)])
            .unwrap();
        assert_eq!(out.inserted, 0);
        let e = OutboxEntity::new("g", "secret");
        s.insert_outbox(&e).unwrap();
        s.confirm_sent(&e.client_message_id, "srv", 3).unwrap();
        assert_eq!(s.redact_confirmed_outbox().unwrap(), 1);
        assert_eq!(
            s.get_outbox(&e.client_message_id).unwrap().unwrap().body,
            ""
        );
    }

    #[test]
    fn members_cascade_and_reconcile_groups() {
        let s = SqliteStore::in_memory().unwrap();
        s.upsert_group(&group("g")).unwrap();
        s.upsert_group(&group("h")).unwrap();
        s.replace_members(
            "g",
            &[MemberEntity {
                group_id: "g".into(),
                user_id: "u".into(),
                display_name: "U".into(),
                phone_e164: None,
                role: Role::Admin,
                joined_at: 1,
            }],
        )
        .unwrap();
        assert_eq!(s.members("g").unwrap().len(), 1);
        assert_eq!(s.get_group("g").unwrap().unwrap().member_count, 1);
        s.reconcile_groups(&[group("g")]).unwrap();
        assert!(s.get_group("h").unwrap().unwrap().is_deleted);
        assert_eq!(s.groups().unwrap().len(), 1);
        s.purge_deleted_groups().unwrap();
        assert!(s.get_group("h").unwrap().is_none());
        s.clear_all().unwrap();
        assert!(s.get_group("g").unwrap().is_none());
        assert!(s.members("g").unwrap().is_empty());
    }
}
