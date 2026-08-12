use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use rho_core::{
    Entry, EntryId, FORMAT_VERSION, Fact, ForkParent, Item, LaneName, LaneStatus, NewEntry,
    NewFact, NewRecord, Record, RecordBody, SessionHeader, SessionId,
};
use serde_json::Value;

use crate::{
    CreateOptions, ForkPoint, RepoFuture, Session, SessionError, SessionMeta, SessionRepo,
    append_entry_to_items, branch_from_items, checked_next_seq, derive_leaf,
    reject_incomplete_tool_turn, session_meta, stamp,
};

/// Process-local reference repository with writer-lock semantics.
#[derive(Clone, Debug, Default)]
pub struct MemoryRepo {
    inner: Arc<Mutex<BTreeMap<SessionId, MemoryFile>>>,
}

#[derive(Clone, Debug)]
struct MemoryFile {
    header: SessionHeader,
    items: Vec<Item>,
    leaf: Option<EntryId>,
    writer_open: bool,
}

impl SessionRepo for MemoryRepo {
    fn create(&self, options: CreateOptions) -> RepoFuture<'_, Box<dyn Session>> {
        Box::pin(async move {
            let id = stamp::session_id();
            let header = SessionHeader {
                v: FORMAT_VERSION,
                id: id.clone(),
                created_at: stamp::timestamp(),
                cwd: options.cwd,
                parent: None,
            };
            let mut files = self.inner.lock().map_err(poisoned)?;
            if files.contains_key(&id) {
                return Err(SessionError::AlreadyExists(id));
            }
            files.insert(
                id.clone(),
                MemoryFile {
                    header: header.clone(),
                    items: Vec::new(),
                    leaf: None,
                    writer_open: true,
                },
            );
            Ok(Box::new(MemorySession {
                repo: Arc::clone(&self.inner),
                id,
                header,
            }) as Box<dyn Session>)
        })
    }

    fn open(&self, id: SessionId) -> RepoFuture<'_, Box<dyn Session>> {
        Box::pin(async move {
            let mut files = self.inner.lock().map_err(poisoned)?;
            let file = files
                .get_mut(&id)
                .ok_or_else(|| SessionError::NotFound(id.clone()))?;
            if file.writer_open {
                return Err(SessionError::Locked(id));
            }
            file.writer_open = true;
            let header = file.header.clone();
            Ok(Box::new(MemorySession {
                repo: Arc::clone(&self.inner),
                id,
                header,
            }) as Box<dyn Session>)
        })
    }

    fn list(&self) -> RepoFuture<'_, Vec<SessionMeta>> {
        Box::pin(async move {
            let files = self.inner.lock().map_err(poisoned)?;
            Ok(files
                .values()
                .map(|file| session_meta(file.header.clone(), &file.items))
                .collect())
        })
    }

    fn delete(&self, id: SessionId) -> RepoFuture<'_, ()> {
        Box::pin(async move {
            let mut files = self.inner.lock().map_err(poisoned)?;
            let file = files
                .get(&id)
                .ok_or_else(|| SessionError::NotFound(id.clone()))?;
            if file.writer_open {
                return Err(SessionError::Locked(id));
            }
            files.remove(&id);
            Ok(())
        })
    }

    fn fork(&self, source: SessionId, at: ForkPoint) -> RepoFuture<'_, Box<dyn Session>> {
        Box::pin(async move {
            let mut files = self.inner.lock().map_err(poisoned)?;
            let source_file = files
                .get(&source)
                .ok_or_else(|| SessionError::NotFound(source.clone()))?;
            let target = match at {
                ForkPoint::Leaf => source_file.leaf.clone(),
                ForkPoint::Entry(entry) => Some(entry),
            };
            let branch =
                branch_from_items(&source_file.items, source_file.leaf.clone(), target.clone())?;
            reject_incomplete_tool_turn(&branch)?;
            let id = stamp::session_id();
            let header = SessionHeader {
                v: FORMAT_VERSION,
                id: id.clone(),
                created_at: stamp::timestamp(),
                cwd: source_file.header.cwd.clone(),
                parent: target.map(|entry| ForkParent {
                    session: source,
                    entry,
                }),
            };
            let items = branch
                .into_iter()
                .enumerate()
                .map(|(index, mut entry)| {
                    entry.seq = u64::try_from(index).unwrap_or(u64::MAX) + 1;
                    entry.op = None;
                    Item::Entry(entry)
                })
                .collect::<Vec<_>>();
            let leaf = derive_leaf(&items);
            files.insert(
                id.clone(),
                MemoryFile {
                    header: header.clone(),
                    items,
                    leaf,
                    writer_open: true,
                },
            );
            Ok(Box::new(MemorySession {
                repo: Arc::clone(&self.inner),
                id,
                header,
            }) as Box<dyn Session>)
        })
    }
}

#[derive(Debug)]
struct MemorySession {
    repo: Arc<Mutex<BTreeMap<SessionId, MemoryFile>>>,
    id: SessionId,
    header: SessionHeader,
}

impl MemorySession {
    fn with_file<T>(
        &self,
        operation: impl FnOnce(&MemoryFile) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let files = self.repo.lock().map_err(poisoned)?;
        let file = files
            .get(&self.id)
            .ok_or_else(|| SessionError::NotFound(self.id.clone()))?;
        operation(file)
    }

    fn with_file_mut<T>(
        &self,
        operation: impl FnOnce(&mut MemoryFile) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let mut files = self.repo.lock().map_err(poisoned)?;
        let file = files
            .get_mut(&self.id)
            .ok_or_else(|| SessionError::NotFound(self.id.clone()))?;
        operation(file)
    }
}

impl Session for MemorySession {
    fn header(&self) -> &SessionHeader {
        &self.header
    }

    fn append_entry(&mut self, entry: NewEntry) -> Result<EntryId, SessionError> {
        self.with_file_mut(|file| {
            let stored = append_entry_to_items(&mut file.items, &mut file.leaf, entry)?;
            Ok(stored.id)
        })
    }

    fn append_record(&mut self, record: NewRecord) -> Result<u64, SessionError> {
        self.with_file_mut(|file| {
            let seq = checked_next_seq(&file.items)?;
            if let RecordBody::LaneMoved { to } = &record.body {
                if !file
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::Entry(entry) if entry.id == *to))
                {
                    return Err(SessionError::UnknownEntry(to.clone()));
                }
                file.leaf = Some(to.clone());
            }
            file.items.push(Item::Record(Record {
                seq,
                lane: record.lane,
                at: record.at,
                body: record.body,
            }));
            Ok(seq)
        })
    }

    fn leaf(&self) -> Option<EntryId> {
        self.with_file(|file| Ok(file.leaf.clone())).unwrap_or(None)
    }

    fn move_leaf(&mut self, to: EntryId, at: rho_core::Timestamp) -> Result<(), SessionError> {
        self.append_record(NewRecord {
            lane: LaneName::main(),
            at,
            body: RecordBody::LaneMoved { to },
        })?;
        Ok(())
    }

    fn branch(&self, from: Option<EntryId>) -> Result<Vec<Entry>, SessionError> {
        self.with_file(|file| branch_from_items(&file.items, file.leaf.clone(), from))
    }

    fn lane_status(&self) -> Result<LaneStatus, SessionError> {
        self.with_file(|file| Ok(rho_core::reduce_lane_status(&file.items, &LaneName::main())))
    }

    fn get_fact(&self, key: &str) -> Option<Value> {
        self.with_file(|file| {
            Ok(file.items.iter().rev().find_map(|item| match item {
                Item::Fact(fact) if fact.key == key => Some(fact.value.clone()),
                Item::Entry(_) | Item::Record(_) | Item::Fact(_) => None,
            }))
        })
        .unwrap_or(None)
    }

    fn set_fact(&mut self, fact: NewFact) -> Result<(), SessionError> {
        self.with_file_mut(|file| {
            let seq = checked_next_seq(&file.items)?;
            file.items.push(Item::Fact(Fact {
                seq,
                at: fact.at,
                key: fact.key,
                value: fact.value,
            }));
            Ok(())
        })
    }

    fn log(&self, after_seq: u64, limit: usize) -> Result<Vec<Item>, SessionError> {
        self.with_file(|file| {
            Ok(file
                .items
                .iter()
                .filter(|item| item.seq() > after_seq)
                .take(limit)
                .cloned()
                .collect())
        })
    }

    fn export_entries(&self) -> Result<Vec<Entry>, SessionError> {
        self.with_file(|file| {
            Ok(file
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Entry(entry) => Some(entry.clone()),
                    Item::Record(_) | Item::Fact(_) => None,
                })
                .collect())
        })
    }
}

impl Drop for MemorySession {
    fn drop(&mut self) {
        if let Ok(mut files) = self.repo.lock()
            && let Some(file) = files.get_mut(&self.id)
        {
            file.writer_open = false;
        }
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> SessionError {
    SessionError::Io(std::io::Error::other("memory repository lock poisoned"))
}

#[cfg(test)]
mod tests {
    use rho_ai::{AssistantMessage, ContentBlock, ModelId, ProviderId, StopReason, Usage};

    use crate::conformance;

    use super::*;

    #[tokio::test]
    async fn passes_shared_conformance_suite() {
        conformance::run(&MemoryRepo::default()).await;
    }

    #[tokio::test]
    async fn fork_rejects_an_incomplete_provider_tool_turn() {
        let repo = MemoryRepo::default();
        let mut session = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .unwrap();
        let id = session.header().id.clone();
        let root = EntryId::from("root");
        session
            .append_entry(NewEntry {
                id: root.clone(),
                parent: None,
                lane: LaneName::main(),
                op: None,
                at: rho_core::Timestamp::from("t1"),
                body: rho_core::EntryBody::Message {
                    message: rho_core::SessionMessage::user("read"),
                },
            })
            .unwrap();
        session
            .append_entry(NewEntry {
                id: EntryId::from("assistant"),
                parent: Some(root),
                lane: LaneName::main(),
                op: None,
                at: rho_core::Timestamp::from("t2"),
                body: rho_core::EntryBody::Message {
                    message: rho_core::SessionMessage::Assistant(AssistantMessage {
                        blocks: vec![ContentBlock::ToolCall {
                            id: rho_ai::ToolCallId::from("call"),
                            name: "read".to_owned(),
                            args: serde_json::json!({}),
                        }],
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                        provider: ProviderId::from("p"),
                        model: ModelId::from("m"),
                    }),
                },
            })
            .unwrap();
        drop(session);

        assert!(matches!(
            repo.fork(id, ForkPoint::Leaf).await,
            Err(SessionError::IncompleteToolTurn { call_ids })
                if call_ids == ["call".to_owned()]
        ));
    }
}
