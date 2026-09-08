//! `BatchReconciler`: decides what to do with an incoming batch of messages
//! for one group, given the current DB state. Pure — no I/O.

use crate::store::MessageEntity;
use std::collections::HashSet;

/// One of our own outbox messages echoed back by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoConfirmation {
    pub client_message_id: String,
    pub server_id: String,
    pub seq: i64,
}

/// Output of [`reconcile`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcilePlan {
    /// Net-new, non-duplicate messages to persist (includes echoes).
    pub to_insert: Vec<MessageEntity>,
    /// Our own sent messages confirmed by the server.
    pub echo_confirmations: Vec<EchoConfirmation>,
    /// IDs of truly new (non-echo) messages, for unread accounting.
    pub new_message_ids: Vec<String>,
    /// Highest seq in the batch (including duplicates), used to advance bookmarks.
    pub max_seq: Option<i64>,
}

impl ReconcilePlan {
    pub fn is_empty(&self) -> bool {
        self.to_insert.is_empty() && self.echo_confirmations.is_empty()
    }
}

/// Current DB state relevant to reconciliation, for a single group.
#[derive(Debug, Default, Clone)]
pub struct ReconcileState {
    /// Message IDs already stored for this group.
    pub existing_ids: HashSet<String>,
    /// Sequence numbers already stored for this group (unique per group).
    pub existing_seqs: HashSet<i64>,
    /// `clientMessageId`s in the outbox that are not yet `CONFIRMED`.
    pub unconfirmed_client_message_ids: HashSet<String>,
}

/// Build a [`ReconcilePlan`] for `batch` (all messages of one group).
///
/// * Messages whose `id` or `seq` already exist are skipped.
/// * Duplicates within the batch are collapsed.
/// * A message whose `clientMessageId` is in the unconfirmed set is our own
///   echo: it is inserted and routed as an [`EchoConfirmation`], not as new.
pub fn reconcile(batch: Vec<MessageEntity>, state: &ReconcileState) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_seqs: HashSet<i64> = HashSet::new();

    for msg in batch {
        plan.max_seq = Some(plan.max_seq.map_or(msg.seq, |m| m.max(msg.seq)));

        if state.existing_ids.contains(&msg.id) || !seen_ids.insert(msg.id.clone()) {
            continue;
        }
        if state.existing_seqs.contains(&msg.seq) || !seen_seqs.insert(msg.seq) {
            continue;
        }

        let is_echo = msg
            .client_message_id
            .as_deref()
            .map(|c| state.unconfirmed_client_message_ids.contains(c))
            .unwrap_or(false);

        if is_echo {
            plan.echo_confirmations.push(EchoConfirmation {
                client_message_id: msg.client_message_id.clone().unwrap_or_default(),
                server_id: msg.id.clone(),
                seq: msg.seq,
            });
        } else {
            plan.new_message_ids.push(msg.id.clone());
        }
        plan.to_insert.push(msg);
    }
    plan.to_insert.sort_by_key(|m| m.seq);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, seq: i64, cmid: Option<&str>) -> MessageEntity {
        MessageEntity {
            id: id.into(),
            group_id: "g".into(),
            seq,
            sender_id: "u".into(),
            body: "b".into(),
            client_message_id: cmid.map(str::to_owned),
            created_at: 0,
        }
    }

    #[test]
    fn splits_new_dup_and_echo() {
        let mut st = ReconcileState::default();
        st.existing_ids.insert("old".into());
        st.existing_seqs.insert(1);
        st.unconfirmed_client_message_ids.insert("c1".into());
        let plan = reconcile(
            vec![
                m("old", 1, None),
                m("a", 2, None),
                m("b", 3, Some("c1")),
                m("a", 2, None),
                m("c", 4, Some("zz")),
            ],
            &st,
        );
        assert_eq!(plan.to_insert.len(), 3);
        assert_eq!(plan.new_message_ids, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(plan.echo_confirmations.len(), 1);
        assert_eq!(plan.echo_confirmations[0].server_id, "b");
        assert_eq!(plan.max_seq, Some(4));
    }
}
