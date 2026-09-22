//! Integration harness (§16.6): drives an embedded Neovim through scripted input with no
//! terminal attached, so tests can assert on the composed grid and the saved JSON. Enabled by
//! the `harness` feature; used by this crate's tests and by the frontend's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nbv_core::{CellKey, Notebook};
use rmpv::Value;

use crate::editor::{self, Editor, EditorEvent, EditorRect, Focus, Viewport};
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

/// How long Neovim has to answer a request, or a wait to come true.
const ANSWER: Duration = Duration::from_secs(10);

/// A script in `dir` that runs the Neovim under test with its own state directory (log, swap
/// files, ShaDa) in `dir`: parallel Neovims on a fresh machine otherwise race to create
/// `~/.local/state/nvim`, and the losers stop at a prompt.
fn isolated_nvim(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("nvim");
    let body = format!(
        "#!/bin/sh\nXDG_STATE_HOME='{}' exec '{}' \"$@\"\n",
        dir.join("state").display(),
        nvim_program().display()
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

pub struct State {
    pub nb: Notebook,
    pub editor: Editor,
    pub ready: bool,
    pub exited: bool,
    pub flushes: usize,
    pub commands: Vec<(String, Value)>,
    pub errors: Vec<String>,
    pub focus: Option<(u64, Focus)>,
    pub viewport: Option<Viewport>,
    pub layouts_done: u64,
    pub written: usize,
}

pub struct Harness {
    pub client: EmbeddedNvim,
    pub state: Arc<Mutex<State>>,
    pub notebook: PathBuf,
    seq: Mutex<u64>,
    _dir: tempfile::TempDir,
    _nvim_dir: tempfile::TempDir,
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
        let nvim_dir = tempfile::tempdir().unwrap();
        let program = isolated_nvim(nvim_dir.path());
        let (client, mut rx) = editor::spawn(program, clean, &extra, &notebook).await.unwrap();
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(State {
            nb,
            editor: {
                let mut e = Editor::new(client.clone(), etx);
                // The fixtures are Python notebooks.
                e.set_language(Some("python".into()));
                e
            },
            ready: false,
            exited: false,
            flushes: 0,
            commands: vec![],
            errors: vec![],
            focus: None,
            viewport: None,
            layouts_done: 0,
            written: 0,
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
                                EditorEvent::Focus { seq, focus } => st.focus = Some((seq, focus)),
                                EditorEvent::Viewport(v) => st.viewport = Some(v),
                                EditorEvent::LayoutDone { seq, error } => {
                                    if let Some(e) = error { st.errors.push(e); }
                                    st.layouts_done = seq;
                                }
                                EditorEvent::Written => st.written += 1,
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
        tokio::time::timeout(ANSWER, editor::handshake(&client, &notebook, opts.width, opts.height))
            .await
            .expect("Neovim did not answer the handshake")
            .unwrap();
        let h = Harness { client, state, notebook, seq: Mutex::new(0), _dir: dir, _nvim_dir: nvim_dir };
        h.wait("startup", |s| s.ready && s.viewport.is_some()).await;
        h.settle().await;
        h
    }

    /// Waits until `pred` holds, failing with the screen after 10 s.
    pub async fn wait(&self, what: &str, pred: impl Fn(&State) -> bool) {
        let deadline = tokio::time::Instant::now() + ANSWER;
        loop {
            if pred(&self.state.lock().unwrap()) {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                let errors = self.state.lock().unwrap().errors.clone();
                let messages =
                    tokio::time::timeout(ANSWER, self.client.exec_lua("return vim.fn.execute('messages')", vec![]))
                        .await;
                panic!("timed out waiting for {what}\nerrors: {errors:?}\nmessages: {messages:?}\n{}", self.screen());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Awaits Neovim's answer to a request, failing with the screen after 10 s: a Neovim
    /// stuck at a prompt would otherwise hang the test forever.
    async fn answer<T>(&self, what: &str, request: impl std::future::Future<Output = T>) -> T {
        match tokio::time::timeout(ANSWER, request).await {
            Ok(v) => v,
            Err(_) => panic!("Neovim did not answer {what:?}\n{}", self.screen()),
        }
    }

    fn next_seq(&self) -> u64 {
        let mut s = self.seq.lock().unwrap();
        *s += 1;
        *s
    }

    /// Stacks every cell's editor window from the top of the screen, one row apart, and
    /// waits until they are on screen. Returns the rectangles.
    pub async fn layout_all(&self, active: Option<&CellKey>) -> Vec<EditorRect> {
        let rects = {
            let st = self.state.lock().unwrap();
            let mut row = 1u16;
            st.nb
                .order()
                .iter()
                .map(|k| {
                    let lines = st.nb.cell(k).unwrap().source().split('\n').count() as u16;
                    let r = EditorRect { key: k.clone(), row, col: 4, width: 40, height: lines, skip: Some(0) };
                    row += lines + 1;
                    r
                })
                .collect::<Vec<_>>()
        };
        self.layout(active, &rects).await;
        rects
    }

    pub async fn layout(&self, active: Option<&CellKey>, rects: &[EditorRect]) {
        let seq = self.next_seq();
        {
            let mut st = self.state.lock().unwrap();
            let State { nb, editor, .. } = &mut *st;
            editor.layout(nb, seq, active, rects);
        }
        self.wait("layout", |s| s.layouts_done >= seq).await;
        let errors = self.state.lock().unwrap().errors.clone();
        assert!(errors.is_empty(), "layout failed: {errors:?}");
        self.settle().await;
    }

    /// Focuses a cell's window (laid out as active first), as nbv does.
    pub async fn enter(&self, key: &CellKey, insert: bool) {
        self.layout_all(Some(key)).await;
        let seq = self.next_seq();
        self.state.lock().unwrap().editor.enter(seq, key, insert);
        let k = key.clone();
        self.wait("focus", move |s| s.focus.as_ref() == Some(&(seq, Focus::Cell(k.clone())))).await;
    }

    pub async fn leave(&self) {
        let seq = self.next_seq();
        self.state.lock().unwrap().editor.leave(seq);
        self.wait("home", move |s| s.focus.as_ref() == Some(&(seq, Focus::Home))).await;
    }

    pub fn key(&self, i: usize) -> CellKey {
        self.state.lock().unwrap().nb.order()[i].clone()
    }

    pub fn source(&self, i: usize) -> String {
        let st = self.state.lock().unwrap();
        st.nb.cell(&st.nb.order()[i]).unwrap().source()
    }

    pub async fn resize(&self, width: usize, height: usize) {
        self.answer("resize", self.client.resize(width, height)).await.unwrap();
        self.settle().await;
    }

    /// Waits until Neovim has processed everything sent so far, including companion calls.
    pub async fn settle(&self) {
        for _ in 0..3 {
            // A round trip through the same channel orders after all earlier requests.
            let _ = self.answer("settle", self.client.request("nvim_eval", vec!["1".into()])).await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    pub async fn keys(&self, keys: &str) {
        self.answer(keys, self.client.input(keys)).await.unwrap();
        self.settle().await;
    }

    pub async fn cmd(&self, cmd: &str) {
        self.answer(cmd, self.client.request("nvim_command", vec![cmd.into()])).await.unwrap();
        self.settle().await;
    }

    /// Runs an Ex command, returning Neovim's error message if it failed.
    pub async fn try_cmd(&self, cmd: &str) -> Result<(), String> {
        let r = self
            .answer(cmd, self.client.request("nvim_command", vec![cmd.into()]))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        self.settle().await;
        r
    }

    pub async fn lua(&self, code: &str) -> Value {
        self.answer(code, self.client.exec_lua(code, vec![])).await.unwrap()
    }

    pub fn screen(&self) -> String {
        let st = self.state.lock().unwrap();
        (0..st.editor.grid.height).map(|r| st.editor.grid.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    /// The screen with every transparent cell shown as `·`.
    pub fn opaque_screen(&self) -> String {
        let st = self.state.lock().unwrap();
        let g = &st.editor.grid;
        (0..g.height)
            .map(|r| {
                (0..g.width)
                    .map(|c| if g.is_transparent(r, c) { "·" } else { g.cell(r, c).text.as_str() })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(&self.notebook).unwrap()).unwrap()
    }

    pub fn dir(&self) -> &Path {
        self._dir.path()
    }
}
