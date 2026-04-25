//! Persistent daemon state (last seen UID, UIDVALIDITY, etc.).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub selected_mailbox: Option<String>,
    pub uidvalidity: Option<u32>,
    pub last_seen_uid: Option<u32>,
    pub highestmodseq: Option<u64>,
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct StateStore {
    path: PathBuf,
    state: State,
}

impl StateStore {
    /// Open or create a state store at `path`. If the file does not exist, returns
    /// a store with default state. If the file exists but is corrupt, returns an error.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StateError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, state })
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Mutate the state and atomically persist the result.
    pub fn update<F>(&mut self, f: F) -> Result<(), StateError>
    where
        F: FnOnce(&mut State),
    {
        f(&mut self.state);
        self.persist()
    }

    fn persist(&self) -> Result<(), StateError> {
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        use std::io::Write;
        tmp.write_all(&bytes)?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(&self.path).map_err(|e| StateError::Io(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_yields_empty_state() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(dir.path().join("state.json")).unwrap();
        assert_eq!(*store.state(), State::default());
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        {
            let mut store = StateStore::open(&path).unwrap();
            store.update(|s| {
                s.selected_mailbox = Some("INBOX".into());
                s.uidvalidity = Some(42);
                s.last_seen_uid = Some(100);
                s.highestmodseq = Some(999);
            }).unwrap();
        }
        let store = StateStore::open(&path).unwrap();
        assert_eq!(store.state().selected_mailbox.as_deref(), Some("INBOX"));
        assert_eq!(store.state().uidvalidity, Some(42));
        assert_eq!(store.state().last_seen_uid, Some(100));
        assert_eq!(store.state().highestmodseq, Some(999));
    }

    #[test]
    fn corrupt_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "this is not json").unwrap();
        let err = StateStore::open(&path);
        assert!(matches!(err, Err(StateError::Json(_))));
    }

    #[test]
    fn write_is_atomic_no_partial_files() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        let mut store = StateStore::open(&path).unwrap();
        store.update(|s| s.last_seen_uid = Some(1)).unwrap();

        // No leftover temp files in the directory
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "exactly one file should remain");
        assert_eq!(entries[0].as_ref().unwrap().file_name(), "state.json");
    }
}