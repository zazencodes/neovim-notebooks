//! Integration harness (§16.6): drives an embedded Neovim through scripted input with no
//! terminal attached, so tests can assert on the composed grid and the saved JSON. Enabled by
//! the `harness` feature; used by this crate's tests and by the frontend's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nbv_core::Notebook;
use rmpv::Value;

use crate::editor::{self, Editor, EditorEvent};
use crate::{EmbeddedNvim, NvimClient};

/// The Neovim under test: `NBV_NVIM`, the repository's `.tools` download, or `nvim`.
pub fn nvim_program() -> PathBuf {
    if let Some(p) = std::env::var_os("NBV_NVIM") {
        return p.into();
    }
    let local = repo_root().join(".tools/nvim-macos-arm64/bin/nvim");
    if local.exists() { local } else { "nvim".into() }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

pub struct State {
    pub nb: Notebook,
    pub editor: Editor,
    pub ready: bool,
    pub exited: bool,
    pub flushes: usize,
    pub commands: Vec<(String, Value)>,
    pub errors: Vec<String>,
}

pub struct Harness {
    pub client: EmbeddedNvim,
    pub state: Arc<Mutex<State>>,
    pub notebook: PathBuf,
    _dir: tempfile::TempDir,
}

pub struct Options {
    /// A user init.lua; `None` runs with `--clean`.
    pub init: Option<String>,
    pub width: usize,
    pub height: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { init: None, width: 80, height: 24 }
    }
}

impl Harness {
    /// Copies a corpus notebook to a temporary directory and opens it.
    pub async fn open(corpus_name: &str, opts: Options) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let notebook = dir.path().canonicalize().unwrap().join(corpus_name);
        std::fs::copy(repo_root().join("tests/corpus").join(corpus_name), &notebook).unwrap();
        Harness::open_path(dir, notebook, opts).await
    }

    pub async fn open_path(dir: tempfile::TempDir, notebook: PathBuf, opts: Options) -> Harness {
        let nb = Notebook::open(&notebook).unwrap();
        let buffer_path = editor::buffer_path(&notebook, &nb);
        let mut extra = vec![];
        let clean = match &opts.init {
            Some(init) => {
                let path = dir.path().join("init.lua");
                std::fs::write(&path, init).unwrap();
                extra.extend(["-u".into(), path.to_string_lossy().into_owned(), "-i".into(), "NONE".into()]);
                false
            }
            None => true,
        };
        let (client, mut rx) = editor::spawn(nvim_program(), clean, &extra, &buffer_path).await.unwrap();
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(State {
            nb,
            editor: Editor::new(client.clone(), etx),
            ready: false,
            exited: false,
            flushes: 0,
            commands: vec![],
            errors: vec![],
        }));
        let s = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        let Some(ev) = ev else { break };
                        let mut st = s.lock().unwrap();
                        let State { nb, editor, .. } = &mut *st;
                        for e in editor.handle(nb, ev) {
                            match e {
                                EditorEvent::Ready => st.ready = true,
                                EditorEvent::Exited => st.exited = true,
                                EditorEvent::Flush => st.flushes += 1,
                                EditorEvent::Command { action, args } => st.commands.push((action, args)),
                                _ => {}
                            }
                        }
                    }
                    e = erx.recv() => {
                        if let Some(e) = e { s.lock().unwrap().errors.push(e.to_string()); }
                    }
                }
            }
        });
        editor::handshake(&client, &buffer_path, opts.width, opts.height).await.unwrap();
        let h = Harness { client, state, notebook, _dir: dir };
        h.wait("startup", |s| s.ready && s.editor.buffer().is_some()).await;
        h.settle().await;
        h
    }

    /// Waits until `pred` holds, failing with the screen after 10 s.
    pub async fn wait(&self, what: &str, pred: impl Fn(&State) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if pred(&self.state.lock().unwrap()) {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}\n{}", self.screen());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn resize(&self, width: usize, height: usize) {
        self.client.resize(width, height).await.unwrap();
        self.settle().await;
    }

    /// Waits until Neovim has processed everything sent so far, including companion calls.
    pub async fn settle(&self) {
        for _ in 0..3 {
            // A round trip through the same channel orders after all earlier requests.
            let _ = self.client.request("nvim_eval", vec!["1".into()]).await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    pub async fn keys(&self, keys: &str) {
        self.client.input(keys).await.unwrap();
        self.settle().await;
    }

    pub async fn cmd(&self, cmd: &str) {
        self.client.request("nvim_command", vec![cmd.into()]).await.unwrap();
        self.settle().await;
    }

    /// Runs an Ex command, returning Neovim's error message if it failed.
    pub async fn try_cmd(&self, cmd: &str) -> Result<(), String> {
        let r = self.client.request("nvim_command", vec![cmd.into()]).await.map(|_| ()).map_err(|e| e.to_string());
        self.settle().await;
        r
    }

    pub async fn lua(&self, code: &str) -> Value {
        self.client.exec_lua(code, vec![]).await.unwrap()
    }

    pub async fn buffer_lines(&self) -> Vec<String> {
        let buf = self.state.lock().unwrap().editor.buffer().unwrap();
        self.client.buffer_lines(buf).await.unwrap()
    }

    pub fn mirror(&self) -> Vec<String> {
        self.state.lock().unwrap().nb.mirror().to_vec()
    }

    pub fn screen(&self) -> String {
        let st = self.state.lock().unwrap();
        (0..st.editor.grid.height).map(|r| st.editor.grid.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    pub fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(&self.notebook).unwrap()).unwrap()
    }

    /// Asserts the mirror equals Neovim's buffer and the document matches it.
    pub async fn assert_in_sync(&self) {
        self.settle().await;
        let lines = self.buffer_lines().await;
        assert_eq!(self.mirror(), lines, "mirror diverged from the buffer");
    }

    pub fn dir(&self) -> &Path {
        self._dir.path()
    }
}
