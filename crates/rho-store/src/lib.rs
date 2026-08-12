//! Session storage traits, conformance tests, and reference backends.
//!
//! The in-memory backend and filesystem shell share the same public contract.

#![allow(clippy::disallowed_methods)]

mod jsonl;
mod memory;
mod stamp;

use std::{future::Future, pin::Pin};

pub use jsonl::{FsyncPolicy, JsonlRepo};
pub use memory::MemoryRepo;
use rho_codec_jsonl::CodecError;
use rho_core::{
    Entry, EntryId, Item, LaneName, LaneStatus, NewEntry, NewFact, NewRecord, RecordBody,
    SessionHeader, SessionId,
};
use serde_json::Value;
use thiserror::Error;

/// Type-erased repository operation.
pub type RepoFuture<'repo, T> =
    Pin<Box<dyn Future<Output = Result<T, SessionError>> + Send + 'repo>>;

/// Options for a new standalone session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateOptions {
    /// Working directory captured in the header.
    pub cwd: String,
}

/// A location in a source session to fork through.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForkPoint {
    /// Fork through the source lane's current leaf.
    Leaf,
    /// Fork through a specific entry.
    Entry(EntryId),
}

/// Cheap repository listing item.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionMeta {
    /// Durable header.
    pub header: SessionHeader,
    /// Current main-lane leaf.
    pub leaf: Option<EntryId>,
    /// Recovery status of the main lane.
    pub status: LaneStatus,
}

/// Typed storage failure.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Filesystem operation failed.
    #[error("session I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Strict JSONL decoding failed.
    #[error("invalid session file: {0}")]
    Codec(#[from] CodecError),
    /// Requested session does not exist.
    #[error("session {0} was not found")]
    NotFound(SessionId),
    /// A generated or supplied identity already exists.
    #[error("session {0} already exists")]
    AlreadyExists(SessionId),
    /// Another writer holds the session lock.
    #[error("session {0} is already open for writing")]
    Locked(SessionId),
    /// An externally supplied identifier is unsafe for filesystem use.
    #[error("invalid session identifier {0:?}")]
    InvalidSessionId(String),
    /// A file's name and durable header disagree about session identity.
    #[error("session file identity mismatch: expected {expected}, found {actual}")]
    HeaderIdMismatch {
        /// Identity implied by the filename or request.
        expected: SessionId,
        /// Identity stored in the header.
        actual: SessionId,
    },
    /// A fork point leaves provider tool calls without results.
    #[error("fork point leaves unresolved tool calls: {call_ids:?}")]
    IncompleteToolTurn {
        /// Provider-issued calls requiring results.
        call_ids: Vec<String>,
    },
    /// Appending would not extend the selected branch.
    #[error("entry parent {actual:?} did not match current leaf {expected:?}")]
    InvalidEntryParent {
        /// Current leaf.
        expected: Option<EntryId>,
        /// Supplied parent.
        actual: Option<EntryId>,
    },
    /// Entry identity was already present.
    #[error("entry {0} already exists")]
    DuplicateEntry(EntryId),
    /// Format v1 only supports the main execution lane.
    #[error("session format v1 does not support lane {0}")]
    UnsupportedLane(LaneName),
    /// Entry identity is absent.
    #[error("entry {0} was not found")]
    UnknownEntry(EntryId),
    /// Sequence allocation overflowed.
    #[error("session sequence exhausted")]
    SequenceExhausted,
}

/// Repository contract shared by all storage implementations.
pub trait SessionRepo: Send + Sync {
    /// Creates and writer-locks a new session.
    fn create(&self, options: CreateOptions) -> RepoFuture<'_, Box<dyn Session>>;

    /// Opens and writer-locks an existing session.
    fn open(&self, id: SessionId) -> RepoFuture<'_, Box<dyn Session>>;

    /// Lists repository snapshots without acquiring writer locks.
    fn list(&self) -> RepoFuture<'_, Vec<SessionMeta>>;

    /// Deletes an unlocked session.
    fn delete(&self, id: SessionId) -> RepoFuture<'_, ()>;

    /// Copies a root-to-point path into a new self-contained session.
    fn fork(&self, source: SessionId, at: ForkPoint) -> RepoFuture<'_, Box<dyn Session>>;
}

/// One writer-locked session handle.
pub trait Session: Send {
    /// Returns the durable session header.
    fn header(&self) -> &SessionHeader;

    /// Appends a pre-stamped transcript entry.
    fn append_entry(&mut self, entry: NewEntry) -> Result<EntryId, SessionError>;

    /// Appends a pre-stamped journal record.
    fn append_record(&mut self, record: NewRecord) -> Result<u64, SessionError>;

    /// Returns the main lane's current leaf.
    fn leaf(&self) -> Option<EntryId>;

    /// Moves the main lane's leaf and journals the move.
    fn move_leaf(&mut self, to: EntryId, at: rho_core::Timestamp) -> Result<(), SessionError>;

    /// Reads one root-to-entry branch, defaulting to the current leaf.
    fn branch(&self, from: Option<EntryId>) -> Result<Vec<Entry>, SessionError>;

    /// Reduces the main lane journal without performing I/O.
    fn lane_status(&self) -> Result<LaneStatus, SessionError>;

    /// Reads the latest value for a fact key.
    fn get_fact(&self, key: &str) -> Option<Value>;

    /// Appends a last-writer-wins fact update.
    fn set_fact(&mut self, fact: NewFact) -> Result<(), SessionError>;

    /// Reads raw interleaved items after a sequence cursor.
    fn log(&self, after_seq: u64, limit: usize) -> Result<Vec<Item>, SessionError>;

    /// Exports transcript entries only, deliberately stripping records.
    fn export_entries(&self) -> Result<Vec<Entry>, SessionError>;
}

fn checked_next_seq(items: &[Item]) -> Result<u64, SessionError> {
    items
        .last()
        .map_or(Some(1), |item| item.seq().checked_add(1))
        .ok_or(SessionError::SequenceExhausted)
}

fn branch_from_items(
    items: &[Item],
    leaf: Option<EntryId>,
    from: Option<EntryId>,
) -> Result<Vec<Entry>, SessionError> {
    let Some(mut cursor) = from.or(leaf) else {
        return Ok(Vec::new());
    };
    let entries = items
        .iter()
        .filter_map(|item| match item {
            Item::Entry(entry) => Some((entry.id.clone(), entry)),
            Item::Record(_) | Item::Fact(_) => None,
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut reversed = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if !seen.insert(cursor.clone()) {
            return Err(SessionError::UnknownEntry(cursor));
        }
        let entry = entries
            .get(&cursor)
            .ok_or_else(|| SessionError::UnknownEntry(cursor.clone()))?;
        reversed.push((*entry).clone());
        let Some(parent) = &entry.parent else {
            break;
        };
        cursor = parent.clone();
    }
    reversed.reverse();
    Ok(reversed)
}

fn derive_leaf(items: &[Item]) -> Option<EntryId> {
    let mut leaf = None;
    for item in items {
        match item {
            Item::Entry(entry) if entry.lane.as_str() == LaneName::MAIN => {
                leaf = Some(entry.id.clone());
            }
            Item::Record(record) if record.lane.as_str() == LaneName::MAIN => {
                if let RecordBody::LaneMoved { to } = &record.body {
                    leaf = Some(to.clone());
                }
            }
            Item::Entry(_) | Item::Record(_) | Item::Fact(_) => {}
        }
    }
    leaf
}

fn session_meta(header: SessionHeader, items: &[Item]) -> SessionMeta {
    SessionMeta {
        header,
        leaf: derive_leaf(items),
        status: rho_core::reduce_lane_status(items, &LaneName::main()),
    }
}

fn reject_incomplete_tool_turn(entries: &[Entry]) -> Result<(), SessionError> {
    let unresolved = rho_core::unresolved_tool_calls(entries)
        .map_err(|error| SessionError::Io(std::io::Error::other(error)))?;
    if unresolved.is_empty() {
        Ok(())
    } else {
        Err(SessionError::IncompleteToolTurn {
            call_ids: unresolved
                .into_iter()
                .map(|call_id| call_id.to_string())
                .collect(),
        })
    }
}

fn append_entry_to_items(
    items: &mut Vec<Item>,
    leaf: &mut Option<EntryId>,
    entry: NewEntry,
) -> Result<Entry, SessionError> {
    if entry.lane.as_str() != LaneName::MAIN {
        return Err(SessionError::UnsupportedLane(entry.lane));
    }
    if items
        .iter()
        .any(|item| matches!(item, Item::Entry(existing) if existing.id == entry.id))
    {
        return Err(SessionError::DuplicateEntry(entry.id));
    }
    if entry.parent != *leaf {
        return Err(SessionError::InvalidEntryParent {
            expected: leaf.clone(),
            actual: entry.parent,
        });
    }
    let stored = Entry {
        seq: checked_next_seq(items)?,
        id: entry.id,
        parent: leaf.clone(),
        lane: entry.lane,
        op: entry.op,
        at: entry.at,
        body: entry.body,
    };
    *leaf = Some(stored.id.clone());
    items.push(Item::Entry(stored.clone()));
    Ok(stored)
}

/// Reusable behavioral checks for any repository implementation.
pub mod conformance {
    use rho_core::{
        EntryBody, EntryId, LaneName, NewEntry, NewFact, NewRecord, OpId, OpIntent, Origin,
        RecordBody, SessionMessage, Timestamp,
    };
    use serde_json::json;

    use super::{CreateOptions, ForkPoint, SessionError, SessionRepo};

    /// Runs backend-independent append, navigation, recovery, fork, fact,
    /// locking, and privacy-export checks.
    pub async fn run(repo: &dyn SessionRepo) {
        let mut session = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .expect("create session");
        let id = session.header().id.clone();
        let root = EntryId::from("00000000-0000-7000-8000-000000000001");
        session
            .append_entry(NewEntry {
                id: root.clone(),
                parent: None,
                lane: LaneName::main(),
                op: None,
                at: Timestamp::from("2026-08-11T00:00:00Z"),
                body: EntryBody::Message {
                    message: SessionMessage::user("root"),
                },
            })
            .expect("append root");
        let unsupported = EntryId::from("00000000-0000-7000-8000-000000000099");
        assert!(matches!(
            session.append_entry(NewEntry {
                id: unsupported,
                parent: Some(root.clone()),
                lane: LaneName::from("background"),
                op: None,
                at: Timestamp::from("2026-08-11T00:00:00Z"),
                body: EntryBody::Message {
                    message: SessionMessage::user("unsupported lane"),
                },
            }),
            Err(SessionError::UnsupportedLane(lane)) if lane.as_str() == "background"
        ));
        assert_eq!(session.leaf(), Some(root.clone()));
        let child = EntryId::from("00000000-0000-7000-8000-000000000002");
        session
            .append_entry(NewEntry {
                id: child.clone(),
                parent: Some(root.clone()),
                lane: LaneName::main(),
                op: None,
                at: Timestamp::from("2026-08-11T00:00:01Z"),
                body: EntryBody::Message {
                    message: SessionMessage::user("child"),
                },
            })
            .expect("append child");
        let branch = session.branch(None).expect("read branch");
        assert_eq!(branch.len(), 2);
        assert!(matches!(
            &branch[0].body,
            EntryBody::Message { message } if message == &SessionMessage::user("root")
        ));
        assert_eq!(session.log(0, 1).expect("read log")[0].seq(), 1);
        session
            .move_leaf(root.clone(), Timestamp::from("2026-08-11T00:00:02Z"))
            .expect("move leaf");
        assert_eq!(session.leaf(), Some(root.clone()));

        session
            .set_fact(NewFact {
                at: Timestamp::from("2026-08-11T00:00:03Z"),
                key: "name".to_owned(),
                value: json!("first"),
            })
            .expect("set fact");
        session
            .set_fact(NewFact {
                at: Timestamp::from("2026-08-11T00:00:04Z"),
                key: "name".to_owned(),
                value: json!("last"),
            })
            .expect("replace fact");
        assert_eq!(session.get_fact("name"), Some(json!("last")));

        let op = OpId::from("00000000-0000-7000-8000-000000000003");
        session
            .append_record(NewRecord {
                lane: LaneName::main(),
                at: Timestamp::from("2026-08-11T00:00:05Z"),
                body: RecordBody::OpStarted {
                    op,
                    intent: OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            })
            .expect("append operation");
        assert!(matches!(
            session.lane_status().expect("lane status"),
            rho_core::LaneStatus::Suspended(_)
        ));
        assert!(
            session
                .export_entries()
                .expect("export entries")
                .iter()
                .all(|entry| matches!(entry.body, EntryBody::Message { .. }))
        );
        assert!(matches!(
            repo.open(id.clone()).await,
            Err(SessionError::Locked(locked)) if locked == id
        ));
        drop(session);

        let reopened = repo.open(id.clone()).await.expect("reopen session");
        assert_eq!(reopened.leaf(), Some(root.clone()));
        assert_eq!(reopened.branch(None).expect("reopened branch").len(), 1);
        drop(reopened);

        let fork = repo
            .fork(id.clone(), ForkPoint::Entry(child.clone()))
            .await
            .expect("fork session");
        assert_eq!(fork.branch(None).expect("fork branch").len(), 2);
        assert_eq!(fork.branch(None).expect("fork branch")[1].id, child);
        assert_eq!(
            fork.header().parent.as_ref().expect("fork lineage").session,
            id
        );
        drop(fork);

        let mut completed = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .expect("create completed session");
        let completed_id = completed.header().id.clone();
        let completed_op = OpId::from("00000000-0000-7000-8000-000000000010");
        completed
            .append_entry(NewEntry {
                id: EntryId::from("00000000-0000-7000-8000-000000000011"),
                parent: None,
                lane: LaneName::main(),
                op: Some(completed_op.clone()),
                at: Timestamp::from("2026-08-11T00:01:00Z"),
                body: EntryBody::Message {
                    message: SessionMessage::user("completed operation"),
                },
            })
            .expect("append completed prompt");
        completed
            .append_record(NewRecord {
                lane: LaneName::main(),
                at: Timestamp::from("2026-08-11T00:01:01Z"),
                body: RecordBody::OpStarted {
                    op: completed_op.clone(),
                    intent: OpIntent::Run,
                    origin: Origin::External,
                    host: None,
                },
            })
            .expect("start completed operation");
        completed
            .append_record(NewRecord {
                lane: LaneName::main(),
                at: Timestamp::from("2026-08-11T00:01:02Z"),
                body: RecordBody::OpFinished {
                    op: completed_op,
                    outcome: rho_core::OpOutcome::Completed,
                },
            })
            .expect("finish completed operation");
        drop(completed);
        let completed_fork = repo
            .fork(completed_id, ForkPoint::Leaf)
            .await
            .expect("fork completed operation");
        assert_eq!(
            completed_fork.lane_status().expect("fork status"),
            rho_core::LaneStatus::Idle
        );
        assert!(
            completed_fork
                .export_entries()
                .expect("fork entries")
                .iter()
                .all(|entry| entry.op.is_none())
        );
        drop(completed_fork);

        let doomed = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .expect("create deletable session");
        let doomed_id = doomed.header().id.clone();
        drop(doomed);
        repo.delete(doomed_id.clone())
            .await
            .expect("delete session");
        assert!(matches!(
            repo.open(doomed_id.clone()).await,
            Err(SessionError::NotFound(missing)) if missing == doomed_id
        ));
    }
}
