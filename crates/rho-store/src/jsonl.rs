use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use rho_codec_jsonl::{decode_session, encode_header, encode_item};
use rho_core::{
    Entry, EntryId, FORMAT_VERSION, Fact, ForkParent, Item, LaneName, LaneStatus, NewEntry,
    NewFact, NewRecord, Record, RecordBody, SessionHeader, SessionId,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    CreateOptions, ForkPoint, RepoFuture, Session, SessionError, SessionMeta, SessionRepo,
    append_entry_to_items, branch_from_items, checked_next_seq, derive_leaf,
    reject_incomplete_tool_turn, session_meta, stamp,
};

/// Durability policy for successful appends.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FsyncPolicy {
    /// Flush every append and fsync operation boundaries.
    #[default]
    OperationBoundary,
    /// Flush and fsync every append.
    EveryAppend,
}

/// Filesystem repository backed by one strict JSONL file per session.
#[derive(Clone, Debug)]
pub struct JsonlRepo {
    directory: PathBuf,
    fsync: FsyncPolicy,
}

impl JsonlRepo {
    /// Creates a repository rooted at `directory` with operation-boundary fsync.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            fsync: FsyncPolicy::OperationBoundary,
        }
    }

    /// Changes the append fsync policy.
    #[must_use]
    pub fn with_fsync_policy(mut self, fsync: FsyncPolicy) -> Self {
        self.fsync = fsync;
        self
    }

    fn session_path(&self, id: &SessionId) -> Result<PathBuf, SessionError> {
        validate_id(id)?;
        Ok(self.directory.join(format!("{id}.jsonl")))
    }

    fn lock_path(&self, id: &SessionId) -> Result<PathBuf, SessionError> {
        validate_id(id)?;
        Ok(self.directory.join(format!("{id}.jsonl.lock")))
    }

    fn create_file(
        &self,
        header: SessionHeader,
        items: Vec<Item>,
    ) -> Result<Box<dyn Session>, SessionError> {
        fs::create_dir_all(&self.directory)?;
        let id = header.id.clone();
        let lock = acquire_lock(&self.lock_path(&id)?, &id)?;
        let path = self.session_path(&id)?;
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(SessionError::AlreadyExists(id));
            }
            Err(error) => return Err(error.into()),
        };
        file.write_all(&encode_header(&header)?)?;
        for item in &items {
            file.write_all(&encode_item(item)?)?;
        }
        file.flush()?;
        file.sync_data()?;
        Ok(Box::new(JsonlSession {
            header,
            items,
            leaf: None,
            file,
            _lock: lock,
            fsync: self.fsync,
        })
        .map_leaf())
    }

    fn read_snapshot(&self, id: &SessionId) -> Result<(SessionHeader, Vec<Item>), SessionError> {
        let path = self.session_path(id)?;
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SessionError::NotFound(id.clone()));
            }
            Err(error) => return Err(error.into()),
        };
        let decoded = decode_session(&bytes)?;
        if decoded.header.id != *id {
            return Err(SessionError::HeaderIdMismatch {
                expected: id.clone(),
                actual: decoded.header.id,
            });
        }
        Ok((decoded.header, decoded.items))
    }
}

trait MapLeaf {
    fn map_leaf(self: Box<Self>) -> Box<dyn Session>;
}

impl MapLeaf for JsonlSession {
    fn map_leaf(mut self: Box<Self>) -> Box<dyn Session> {
        self.leaf = derive_leaf(&self.items);
        self
    }
}

impl SessionRepo for JsonlRepo {
    fn create(&self, options: CreateOptions) -> RepoFuture<'_, Box<dyn Session>> {
        Box::pin(async move {
            let header = SessionHeader {
                v: FORMAT_VERSION,
                id: stamp::session_id(),
                created_at: stamp::timestamp(),
                cwd: options.cwd,
                parent: None,
            };
            self.create_file(header, Vec::new())
        })
    }

    fn open(&self, id: SessionId) -> RepoFuture<'_, Box<dyn Session>> {
        Box::pin(async move {
            fs::create_dir_all(&self.directory)?;
            let lock = acquire_lock(&self.lock_path(&id)?, &id)?;
            let path = self.session_path(&id)?;
            let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SessionError::NotFound(id));
                }
                Err(error) => return Err(error.into()),
            };
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let decoded = decode_session(&bytes)?;
            if decoded.header.id != id {
                return Err(SessionError::InvalidSessionId(
                    decoded.header.id.to_string(),
                ));
            }
            if decoded.had_torn_tail {
                file.set_len(u64::try_from(decoded.valid_up_to).map_err(|_| {
                    SessionError::Io(std::io::Error::other("session file is too large"))
                })?)?;
            }
            file.seek(SeekFrom::End(0))?;
            let leaf = derive_leaf(&decoded.items);
            Ok(Box::new(JsonlSession {
                header: decoded.header,
                items: decoded.items,
                leaf,
                file,
                _lock: lock,
                fsync: self.fsync,
            }) as Box<dyn Session>)
        })
    }

    fn list(&self) -> RepoFuture<'_, Vec<SessionMeta>> {
        Box::pin(async move {
            match fs::read_dir(&self.directory) {
                Ok(entries) => {
                    let mut sessions = Vec::new();
                    for entry in entries {
                        let path = entry?.path();
                        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                            continue;
                        }
                        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                            return Err(SessionError::InvalidSessionId(path.display().to_string()));
                        };
                        let id = SessionId::from(stem);
                        validate_id(&id)?;
                        let decoded = decode_session(&fs::read(path)?)?;
                        if decoded.header.id != id {
                            return Err(SessionError::HeaderIdMismatch {
                                expected: id,
                                actual: decoded.header.id,
                            });
                        }
                        sessions.push(session_meta(decoded.header, &decoded.items));
                    }
                    sessions.sort_by(|left, right| left.header.id.cmp(&right.header.id));
                    Ok(sessions)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(error) => Err(error.into()),
            }
        })
    }

    fn delete(&self, id: SessionId) -> RepoFuture<'_, ()> {
        Box::pin(async move {
            let _lock = acquire_lock(&self.lock_path(&id)?, &id)?;
            match fs::remove_file(self.session_path(&id)?) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(SessionError::NotFound(id))
                }
                Err(error) => Err(error.into()),
            }
        })
    }

    fn fork(&self, source: SessionId, at: ForkPoint) -> RepoFuture<'_, Box<dyn Session>> {
        Box::pin(async move {
            let (source_header, source_items) = self.read_snapshot(&source)?;
            let source_leaf = derive_leaf(&source_items);
            let target = match at {
                ForkPoint::Leaf => source_leaf.clone(),
                ForkPoint::Entry(entry) => Some(entry),
            };
            let branch = branch_from_items(&source_items, source_leaf, target.clone())?;
            reject_incomplete_tool_turn(&branch)?;
            let id = stamp::session_id();
            let header = SessionHeader {
                v: FORMAT_VERSION,
                id,
                created_at: stamp::timestamp(),
                cwd: source_header.cwd,
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
                    entry.source_queue = None;
                    Item::Entry(entry)
                })
                .collect();
            self.create_file(header, items)
        })
    }
}

#[derive(Debug)]
struct JsonlSession {
    header: SessionHeader,
    items: Vec<Item>,
    leaf: Option<EntryId>,
    file: File,
    _lock: File,
    fsync: FsyncPolicy,
}

impl JsonlSession {
    fn append_item(&mut self, item: Item, boundary: bool) -> Result<(), SessionError> {
        self.file.write_all(&encode_item(&item)?)?;
        self.file.flush()?;
        if self.fsync == FsyncPolicy::EveryAppend
            || (self.fsync == FsyncPolicy::OperationBoundary && boundary)
        {
            self.file.sync_data()?;
        }
        self.items.push(item);
        Ok(())
    }
}

impl Session for JsonlSession {
    fn header(&self) -> &SessionHeader {
        &self.header
    }

    fn append_entry(&mut self, entry: NewEntry) -> Result<EntryId, SessionError> {
        let mut candidate_items = self.items.clone();
        let mut candidate_leaf = self.leaf.clone();
        let stored = append_entry_to_items(&mut candidate_items, &mut candidate_leaf, entry)?;
        self.append_item(Item::Entry(stored.clone()), false)?;
        self.leaf = candidate_leaf;
        Ok(stored.id)
    }

    fn append_record(&mut self, record: NewRecord) -> Result<u64, SessionError> {
        let seq = checked_next_seq(&self.items)?;
        let moved_to = match &record.body {
            RecordBody::LaneMoved { to } => {
                if !self
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::Entry(entry) if entry.id == *to))
                {
                    return Err(SessionError::UnknownEntry(to.clone()));
                }
                Some(to.clone())
            }
            _ => None,
        };
        let boundary = is_sync_boundary(&record.body);
        self.append_item(
            Item::Record(Record {
                seq,
                lane: record.lane,
                at: record.at,
                body: record.body,
            }),
            boundary,
        )?;
        if moved_to.is_some() {
            self.leaf = moved_to;
        }
        Ok(seq)
    }

    fn leaf(&self) -> Option<EntryId> {
        self.leaf.clone()
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
        branch_from_items(&self.items, self.leaf.clone(), from)
    }

    fn lane_status(&self) -> Result<LaneStatus, SessionError> {
        Ok(rho_core::reduce_lane_status(&self.items, &LaneName::main()))
    }

    fn get_fact(&self, key: &str) -> Option<Value> {
        self.items.iter().rev().find_map(|item| match item {
            Item::Fact(fact) if fact.key == key => Some(fact.value.clone()),
            Item::Entry(_) | Item::Record(_) | Item::Fact(_) => None,
        })
    }

    fn set_fact(&mut self, fact: NewFact) -> Result<(), SessionError> {
        let seq = checked_next_seq(&self.items)?;
        self.append_item(
            Item::Fact(Fact {
                seq,
                at: fact.at,
                key: fact.key,
                value: fact.value,
            }),
            false,
        )
    }

    fn log(&self, after_seq: u64, limit: usize) -> Result<Vec<Item>, SessionError> {
        Ok(self
            .items
            .iter()
            .filter(|item| item.seq() > after_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    fn export_entries(&self) -> Result<Vec<Entry>, SessionError> {
        Ok(self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Entry(entry) => Some(entry.clone()),
                Item::Record(_) | Item::Fact(_) => None,
            })
            .collect())
    }
}

fn validate_id(id: &SessionId) -> Result<(), SessionError> {
    let parsed = Uuid::parse_str(id.as_str())
        .map_err(|_| SessionError::InvalidSessionId(id.as_str().to_owned()))?;
    if parsed.get_version_num() != 7 {
        return Err(SessionError::InvalidSessionId(id.as_str().to_owned()));
    }
    Ok(())
}

fn is_sync_boundary(body: &RecordBody) -> bool {
    matches!(
        body,
        RecordBody::OpStarted { .. }
            | RecordBody::OpFinished { .. }
            | RecordBody::AbortRequested { .. }
            | RecordBody::ToolStarted { .. }
    )
}

fn acquire_lock(path: &Path, id: &SessionId) -> Result<File, SessionError> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(SessionError::Locked(id.clone())),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use crate::conformance;

    use super::*;

    fn temp_repo() -> (PathBuf, JsonlRepo) {
        let path = std::env::temp_dir().join(format!("rho-store-test-{}", Uuid::now_v7()));
        (path.clone(), JsonlRepo::new(path))
    }

    #[tokio::test]
    async fn passes_shared_conformance_suite() {
        let (path, repo) = temp_repo();
        conformance::run(&repo).await;
        fs::remove_dir_all(path).expect("remove test repository");
    }

    #[tokio::test]
    async fn open_truncates_only_an_incomplete_tail() {
        let (path, repo) = temp_repo();
        let session = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .unwrap();
        let id = session.header().id.clone();
        drop(session);
        let session_path = repo.session_path(&id).unwrap();
        let clean_len = fs::metadata(&session_path).unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(&session_path)
            .unwrap()
            .write_all(br#"{"t":"entry","seq":1"#)
            .unwrap();
        let session = repo.open(id).await.unwrap();
        drop(session);
        assert_eq!(fs::metadata(session_path).unwrap().len(), clean_len);
        fs::remove_dir_all(path).expect("remove test repository");
    }

    #[tokio::test]
    async fn path_like_session_ids_are_rejected() {
        let (path, repo) = temp_repo();
        assert!(matches!(
            repo.open(SessionId::from("../escape")).await,
            Err(SessionError::InvalidSessionId(_))
        ));
        if path.exists() {
            fs::remove_dir_all(path).expect("remove test repository");
        }
    }

    #[test]
    fn tool_start_is_fsynced_before_the_side_effect() {
        assert!(is_sync_boundary(&RecordBody::ToolStarted {
            op: rho_core::OpId::from("op"),
            call_id: rho_ai::ToolCallId::from("call"),
            name: "write".to_owned(),
            effective_args: serde_json::json!({}),
            replay: rho_core::ReplaySafety::Never,
        }));
    }

    #[tokio::test]
    async fn filename_and_header_identity_must_match_for_list_and_fork() {
        let (path, repo) = temp_repo();
        let session = repo
            .create(CreateOptions {
                cwd: "/workspace".to_owned(),
            })
            .await
            .unwrap();
        let original = session.header().id.clone();
        drop(session);
        let substituted = SessionId::from(Uuid::now_v7().to_string());
        fs::rename(
            repo.session_path(&original).unwrap(),
            repo.session_path(&substituted).unwrap(),
        )
        .unwrap();

        assert!(matches!(
            repo.list().await,
            Err(SessionError::HeaderIdMismatch { expected, actual })
                if expected == substituted && actual == original
        ));
        assert!(matches!(
            repo.fork(substituted.clone(), ForkPoint::Leaf).await,
            Err(SessionError::HeaderIdMismatch { expected, actual })
                if expected == substituted && actual == original
        ));
        fs::remove_dir_all(path).expect("remove test repository");
    }
}
