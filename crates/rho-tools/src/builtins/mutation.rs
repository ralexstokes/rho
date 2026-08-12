use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
};

use tokio::sync::{Mutex, OwnedMutexGuard};

type LockMap = HashMap<PathBuf, Weak<Mutex<()>>>;

fn locks() -> &'static Mutex<LockMap> {
    static LOCKS: OnceLock<Mutex<LockMap>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) async fn lock(path: &Path) -> OwnedMutexGuard<()> {
    let key = canonical_key(path).await;
    let file_lock = {
        let mut registered = locks().lock().await;
        registered.retain(|_, file_lock| file_lock.strong_count() > 0);
        if let Some(file_lock) = registered.get(&key).and_then(Weak::upgrade) {
            file_lock
        } else {
            let file_lock = Arc::new(Mutex::new(()));
            registered.insert(key, Arc::downgrade(&file_lock));
            file_lock
        }
    };
    file_lock.lock_owned().await
}

async fn canonical_key(path: &Path) -> PathBuf {
    if let Ok(path) = tokio::fs::canonicalize(path).await {
        return path;
    }
    let mut ancestor = path;
    let mut missing = Vec::new();
    while let Some(name) = ancestor.file_name() {
        missing.push(name.to_owned());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
        if let Ok(mut canonical) = tokio::fs::canonicalize(ancestor).await {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
    }
    path.to_path_buf()
}
