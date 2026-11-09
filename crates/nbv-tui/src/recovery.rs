//! One in-flight background recovery write, independent of Neovim's mode. Lifecycle
//! operations wait for it before cleanup so an old write cannot resurrect a saved snapshot.

use nbv_core::Notebook;
use nbv_core::recovery::{RecoveryStore, SnapshotCache};
use nbv_nvim::NvimEvent;
use rmpv::Value;
use tokio::task::JoinHandle;

/// Reject writes/reloads before Editor::handle can mutate disk state or clear recovery.
/// A plugin timer can issue these requests even while an asynchronous picker is unresolved.
pub(crate) fn gate_pending_request(event: NvimEvent) -> Option<NvimEvent> {
    match event {
        NvimEvent::Request { name, reply, .. } if matches!(name.as_str(), "nbv_commit" | "nbv_reload") => {
            let _ = reply.send(Value::Map(vec![(
                "error".into(),
                "choose whether to recover or discard the pending notebook snapshot before saving or reloading".into(),
            )]));
            None
        }
        event => Some(event),
    }
}

pub(crate) struct Recovery {
    pub store: RecoveryStore,
    cache: SnapshotCache,
    revision: u64,
    persisted: u64,
    writing: Option<(u64, JoinHandle<anyhow::Result<()>>)>,
    retired: Vec<RecoveryStore>,
}

impl Recovery {
    pub fn new(store: RecoveryStore) -> Self {
        Self { store, cache: SnapshotCache::default(), revision: 0, persisted: 0, writing: None, retired: vec![] }
    }

    pub fn changed(&mut self) {
        self.revision += 1;
    }

    async fn finish(&mut self) -> anyhow::Result<()> {
        if let Some((revision, job)) = self.writing.take() {
            job.await??;
            self.persisted = revision;
        }
        Ok(())
    }

    fn start(&mut self, nb: &Notebook) {
        let snapshot = self.cache.capture(nb);
        let store = self.store.clone();
        self.writing = Some((
            self.revision,
            tokio::task::spawn_blocking(move || {
                store.persist(snapshot)?;
                Ok(())
            }),
        ));
    }

    /// Called at most once a second. Busy writes coalesce subsequent edits until the next
    /// tick; a busy disk cannot accumulate an unbounded queue of notebook copies.
    pub async fn tick(&mut self, nb: &Notebook) -> anyhow::Result<()> {
        if self.writing.as_ref().is_some_and(|(_, job)| job.is_finished()) {
            self.finish().await?;
        }
        if self.writing.is_none() && self.revision != self.persisted {
            self.start(nb);
        }
        self.collect_retired().await
    }

    /// Drain the latest revision on an unexpected but orderly application shutdown.
    pub async fn flush(&mut self, nb: &Notebook) -> anyhow::Result<()> {
        self.finish().await?;
        if self.revision != self.persisted {
            self.start(nb);
            self.finish().await?;
        }
        self.collect_retired().await
    }

    /// Common shutdown epilogue, including renderer failures. The original error remains
    /// the cause when recovery cleanup itself also fails.
    pub async fn finish_session(
        &mut self,
        nb: &Notebook,
        discard: bool,
        result: anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let cleanup = if discard && result.is_ok() { self.clear().await } else { self.flush(nb).await };
        match (result, cleanup) {
            (Err(original), Err(cleanup)) => {
                let context = format!("{original}; recovery cleanup also failed: {cleanup}");
                Err(original.context(context))
            }
            (Err(original), _) => Err(original),
            (Ok(()), cleanup) => cleanup,
        }
    }

    pub async fn clear(&mut self) -> anyhow::Result<()> {
        // Even a failed in-flight write must finish before deleting its files. Surface that
        // failure, but still attempt cleanup of data the user saved or explicitly discarded.
        let prior = self.finish().await;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.clear()).await??;
        self.persisted = self.revision;
        self.cache = SnapshotCache::default();
        self.collect_retired().await?;
        prior
    }

    pub async fn renamed(&mut self, store: RecoveryStore, nb: &Notebook, dirty: bool) -> anyhow::Result<()> {
        self.finish().await?;
        self.retired.push(std::mem::replace(&mut self.store, store));
        // A snapshot at the new association must be durable before deleting the old one.
        if dirty {
            self.changed();
            self.flush(nb).await?;
        } else {
            self.collect_retired().await?;
        }
        Ok(())
    }

    async fn collect_retired(&mut self) -> anyhow::Result<()> {
        if self.writing.is_some() || self.revision != self.persisted {
            return Ok(());
        }
        while let Some(store) = self.retired.last().cloned() {
            tokio::task::spawn_blocking(move || store.clear()).await??;
            self.retired.pop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nbv_core::CommitOptions;
    use nbv_nvim::harness::{Harness, Options};
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Notebook, RecoveryStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ipynb");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "nbformat": 4, "nbformat_minor": 5, "metadata": {},
                "cells": [{"id": "code", "cell_type": "code", "source": "saved", "metadata": {},
                    "execution_count": null, "outputs": []}]
            }))
            .unwrap(),
        )
        .unwrap();
        let path = path.canonicalize().unwrap();
        let nb = Notebook::open(&path).unwrap();
        let store = RecoveryStore::at(&dir.path().join("state"), &path).unwrap();
        (dir, nb, store)
    }

    #[tokio::test]
    async fn in_flight_writes_coalesce_and_flush_keeps_the_latest_revision() {
        let (_dir, mut nb, store) = fixture();
        let mut recovery = Recovery::new(store);
        let key = nb.order()[0].clone();
        nb.set_source(&key, "first");
        recovery.changed();
        recovery.tick(&nb).await.unwrap();
        assert!(recovery.writing.is_some());
        nb.set_source(&key, "latest");
        recovery.changed();
        recovery.flush(&nb).await.unwrap();
        assert_eq!(recovery.store.load().unwrap().unwrap().cell(&key).unwrap().source(), "latest");
        recovery.tick(&nb).await.unwrap();
        assert!(recovery.writing.is_none(), "unchanged documents schedule no encoding or writes");
    }

    #[tokio::test]
    async fn save_cleanup_waits_for_old_writes_and_cannot_resurrect_a_snapshot() {
        let (_dir, mut nb, store) = fixture();
        let mut recovery = Recovery::new(store);
        nb.set_source(&nb.order()[0].clone(), "mine");
        recovery.changed();
        recovery.tick(&nb).await.unwrap();
        nb.commit(CommitOptions::default()).unwrap();
        recovery.clear().await.unwrap();
        recovery.tick(&nb).await.unwrap();
        assert!(recovery.writing.is_none());
        assert!(recovery.store.load().unwrap().is_none());
        assert_eq!(Notebook::open(nb.path()).unwrap().to_json(), nb.to_json());
    }

    #[tokio::test]
    async fn accepted_external_reload_cleans_up_in_flight_unsaved_data() {
        let (_dir, mut nb, store) = fixture();
        let mut recovery = Recovery::new(store);
        let key = nb.order()[0].clone();
        nb.set_source(&key, "mine");
        recovery.changed();
        recovery.tick(&nb).await.unwrap();
        let mut outside = Notebook::open(nb.path()).unwrap();
        outside.set_source(&key, "outside");
        outside.commit(CommitOptions::default()).unwrap();
        nb.reload().unwrap();
        recovery.clear().await.unwrap();
        recovery.tick(&nb).await.unwrap();
        assert!(recovery.store.load().unwrap().is_none());
        assert_eq!(nb.cell(&key).unwrap().source(), "outside");
    }

    #[tokio::test]
    async fn rename_moves_dirty_recovery_and_its_saved_baseline() {
        let (dir, mut nb, store) = fixture();
        let old = store.clone();
        let mut recovery = Recovery::new(store);
        let key = nb.order()[0].clone();
        nb.set_source(&key, "mine");
        recovery.changed();
        recovery.tick(&nb).await.unwrap();
        nb.rename("renamed.ipynb").unwrap();
        let new = RecoveryStore::at(&dir.path().join("state"), nb.path()).unwrap();
        recovery.renamed(new, &nb, true).await.unwrap();
        assert!(old.load().unwrap().is_none());
        let mut recovered = recovery.store.load().unwrap().unwrap();
        assert_eq!(recovered.path(), nb.path());
        assert_eq!(recovered.cell(&key).unwrap().source(), "mine");
        recovered.commit(CommitOptions::default()).unwrap();
        assert_eq!(Notebook::open(nb.path()).unwrap().cell(&key).unwrap().source(), "mine");
        recovery.clear().await.unwrap();
        assert!(recovery.store.load().unwrap().is_none());
    }

    #[tokio::test]
    async fn pending_recovery_rejects_commit_and_reload_before_editor_dispatch() {
        let (_dir, mut nb, store) = fixture();
        nb.set_source(&nb.order()[0].clone(), "pending unsaved work");
        store.persist(SnapshotCache::default().capture(&nb)).unwrap();
        let bytes = std::fs::read(store.manifest_path()).unwrap();
        let disk = std::fs::read(nb.path()).unwrap();
        for name in ["nbv_commit", "nbv_reload"] {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let event = NvimEvent::Request { name: name.into(), args: vec![], reply };
            assert!(gate_pending_request(event).is_none());
            let answer = answer.await.unwrap();
            assert!(
                nbv_nvim::editor::field(&answer, "error").unwrap().as_str().unwrap().contains("recover or discard")
            );
            assert_eq!(std::fs::read(store.manifest_path()).unwrap(), bytes);
            assert_eq!(std::fs::read(nb.path()).unwrap(), disk);
        }
    }

    #[tokio::test]
    async fn renderer_failure_drains_latest_edits_before_returning_the_original_error() {
        let (_dir, mut nb, store) = fixture();
        let mut recovery = Recovery::new(store);
        let key = nb.order()[0].clone();
        nb.set_source(&key, "latest edit before first tick");
        recovery.changed();
        // Inject the error produced by a renderer with a closed terminal. No timer has run.
        let render = || -> anyhow::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "terminal draw failed").into())
        };
        let error = recovery.finish_session(&nb, true, render()).await.unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "terminal draw failed");
        assert_eq!(
            recovery.store.load().unwrap().unwrap().cell(&key).unwrap().source(),
            "latest edit before first tick"
        );
    }

    #[tokio::test]
    async fn renderer_error_remains_visible_when_recovery_cleanup_also_fails() {
        let (_dir, nb, store) = fixture();
        std::fs::write(store.manifest_path().parent().unwrap(), "blocked directory").unwrap();
        let mut recovery = Recovery::new(store);
        recovery.changed();
        let render_error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "original terminal error");
        let error = recovery.finish_session(&nb, false, Err(render_error.into())).await.unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("original terminal error"));
        assert!(error.to_string().contains("recovery cleanup also failed"));
    }

    #[tokio::test]
    async fn insert_mode_changes_reach_recovery_without_a_write_or_mode_change() {
        let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
        let key = h.key(1);
        h.enter(&key, false).await;
        let before = h.source(1);
        let (head, tail) = before.split_once('\n').unwrap();
        let expected = format!("{head} # still in insert mode\n{tail}");
        h.keys("A # still in insert mode").await;
        h.wait("insert mode source mirrored", |s| s.nb.cell(&key).unwrap().source() == expected).await;
        assert_eq!(h.lua("return vim.fn.mode()").await.as_str(), Some("i"));
        let dir = tempfile::tempdir().unwrap();
        let store = RecoveryStore::at(dir.path(), &h.notebook).unwrap();
        let snapshot = {
            let state = h.state.lock().unwrap();
            SnapshotCache::default().capture(&state.nb)
        };
        let writer = store.clone();
        tokio::task::spawn_blocking(move || writer.persist(snapshot)).await.unwrap().unwrap();
        let recovered = store.load().unwrap().unwrap();
        assert_eq!(recovered.cell(&key).unwrap().source(), expected);
        assert!(!Notebook::open(&h.notebook).unwrap().cell(&key).unwrap().source().contains("still in insert"));
        assert_eq!(h.lua("return vim.fn.mode()").await.as_str(), Some("i"));
    }
}
