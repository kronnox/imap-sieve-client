//! Loads and (optionally) hot-reloads a Sieve script file.

use crate::sieve_engine::{CompiledScript, SieveEngine, SieveError};
use arc_swap::ArcSwap;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("compile: {0}")]
    Compile(#[from] SieveError),
}

pub struct ScriptHandle {
    current: Arc<ArcSwap<CompiledScript>>,
}

impl ScriptHandle {
    pub fn current(&self) -> Arc<CompiledScript> {
        self.current.load_full()
    }
}

pub struct ScriptLoader<E: SieveEngine> {
    engine: E,
    path: PathBuf,
    handle: Arc<ArcSwap<CompiledScript>>,
}

impl<E: SieveEngine> ScriptLoader<E> {
    /// Compile the script at `path` once and return a `ScriptHandle` plus this loader.
    pub fn load(engine: E, path: impl Into<PathBuf>) -> Result<(Self, ScriptHandle), LoaderError> {
        let path = path.into();
        let text = std::fs::read_to_string(&path)?;
        let compiled = engine.compile(&text)?;
        let handle = Arc::new(ArcSwap::from_pointee(compiled));
        let loader = Self {
            engine,
            path,
            handle: handle.clone(),
        };
        Ok((loader, ScriptHandle { current: handle }))
    }

    /// Re-read and recompile. On compile failure, leaves the existing script in place.
    pub fn reload(&self) -> Result<(), LoaderError> {
        let text = std::fs::read_to_string(&self.path)?;
        let compiled = self.engine.compile(&text)?;
        self.handle.store(Arc::new(compiled));
        Ok(())
    }
}

/// RAII guard for the file watcher. Drop it to stop watching and join the worker.
pub struct WatcherGuard {
    // `Option` so `Drop` can take ownership.
    watcher: Option<RecommendedWatcher>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        // Drop the watcher first so the channel sender side is closed.
        drop(self.watcher.take());
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl<E: SieveEngine + Send + Sync + 'static> ScriptLoader<E> {
    /// Spawn a notify-based watcher that reloads on file change.
    /// Returns a guard; dropping it stops the watcher and joins the worker.
    pub fn spawn_watcher(self) -> Result<WatcherGuard, LoaderError> {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel();
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        watcher
            .watch(&self.path, RecursiveMode::NonRecursive)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let loader = self;
        let thread = std::thread::Builder::new()
            .name("sieve-watcher".into())
            .spawn(move || {
                while let Ok(res) = rx.recv() {
                    match res {
                        Ok(event) => {
                            if matches!(
                                event.kind,
                                EventKind::Modify(_) | EventKind::Create(_)
                            ) {
                                if let Err(e) = loader.reload() {
                                    tracing::warn!(error = %e, "sieve script reload failed; keeping previous script");
                                } else {
                                    tracing::info!("sieve script reloaded");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "watcher reported error; continuing");
                        }
                    }
                }
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(WatcherGuard {
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sieve_engine::SieveEngineImpl;
    use tempfile::TempDir;

    #[test]
    fn loads_initial_script() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rules.sieve");
        std::fs::write(&path, "keep;").unwrap();

        let (loader, handle) = ScriptLoader::load(SieveEngineImpl::new(), &path).unwrap();
        let _ = handle.current();
        let _ = loader;
    }

    #[test]
    fn reload_swaps_compiled_script() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rules.sieve");
        std::fs::write(&path, "keep;").unwrap();
        let (loader, handle) = ScriptLoader::load(SieveEngineImpl::new(), &path).unwrap();
        let first = Arc::as_ptr(&handle.current());

        std::fs::write(&path, "discard;").unwrap();
        loader.reload().unwrap();

        let second = Arc::as_ptr(&handle.current());
        assert_ne!(first, second, "Arc pointer should change after reload");
    }

    #[test]
    fn reload_compile_error_keeps_old_script() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rules.sieve");
        std::fs::write(&path, "keep;").unwrap();
        let (loader, handle) = ScriptLoader::load(SieveEngineImpl::new(), &path).unwrap();
        let original = Arc::as_ptr(&handle.current());

        std::fs::write(&path, "garbage that won't compile").unwrap();
        assert!(loader.reload().is_err());

        let after = Arc::as_ptr(&handle.current());
        assert_eq!(original, after, "old script must remain on compile failure");
    }

    #[tokio::test]
    async fn watcher_reloads_on_file_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rules.sieve");
        std::fs::write(&path, "keep;").unwrap();
        let (loader, handle) = ScriptLoader::load(SieveEngineImpl::new(), &path).unwrap();
        let initial = Arc::as_ptr(&handle.current());

        let _watcher = loader.spawn_watcher().expect("watcher");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::fs::write(&path, "discard;").unwrap();

        // Give the watcher time to pick up the change
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if Arc::as_ptr(&handle.current()) != initial {
                return;
            }
        }
        panic!("watcher did not pick up file change");
    }
}
