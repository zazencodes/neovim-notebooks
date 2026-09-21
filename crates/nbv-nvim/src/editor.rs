//! The cell buffer adapter (core ↔ Neovim) and the editor driver that routes Neovim's events:
//! redraws to the grid, buffer events to cell sources, the companion's writes and reloads to
//! persistence (§9), and everything else to the application.
//!
//! Every cell is edited in its own Neovim buffer, shown in its own floating window (§7). Rust
//! mirrors each buffer's lines, so a cell's source is always the text of its buffer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nbv_core::{CellKey, CellKind, CommitOptions, Notebook};
use rmpv::Value;
use tokio::sync::mpsc;

use crate::client::{NvimClient, NvimError, NvimEvent, SpawnOptions, strings};
use crate::grid::Grid;

/// The companion (R6), embedded in the binary and loaded pre-config.
pub const COMPANION: &str = include_str!("../../../lua/nbv/init.lua");

/// The home buffer's name: the notebook's path under the `nbv://` scheme (§9.1).
pub fn home_name(notebook: &Path) -> String {
    format!("nbv://{}", notebook.display())
}

/// A cell buffer's name: the notebook path, the cell's key and an extension for its type, in
/// the notebook's directory, so language servers and root detection work (§9.1). `language`
/// is the kernel's; without a kernel, code cells have neither extension nor filetype.
pub fn cell_buffer_name(nb: &Notebook, key: &CellKey, kind: CellKind, language: Option<&str>) -> PathBuf {
    let ext = match kind {
        CellKind::Code => match language {
            Some("python") => ".py",
            Some("r") => ".r",
            Some("julia") => ".jl",
            _ => "",
        },
        CellKind::Markdown => ".md",
        CellKind::Raw => ".txt",
    };
    let mut s = nb.path().as_os_str().to_owned();
    s.push(format!(".{key}{ext}"));
    PathBuf::from(s)
}

pub fn filetype(kind: CellKind, language: Option<&str>) -> String {
    match kind {
        CellKind::Code => language.unwrap_or("").into(),
        CellKind::Markdown => "markdown".into(),
        CellKind::Raw => "text".into(),
    }
}

/// Neovim calls, issued in order by one worker so the event loop never waits on Neovim.
#[derive(Clone)]
pub struct Calls {
    tx: mpsc::UnboundedSender<(String, Vec<Value>)>,
}

impl Calls {
    pub fn start<C: NvimClient>(client: C, errors: mpsc::UnboundedSender<NvimError>) -> Calls {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Vec<Value>)>();
        tokio::spawn(async move {
            while let Some((method, args)) = rx.recv().await {
                if let Err(e) = client.request(&method, args).await {
                    let _ = errors.send(e);
                }
            }
        });
        Calls { tx }
    }

    pub fn request(&self, method: &str, args: Vec<Value>) {
        let _ = self.tx.send((method.into(), args));
    }

    pub fn lua(&self, code: &str, args: Vec<Value>) {
        self.request("nvim_exec_lua", vec![code.into(), Value::Array(args)]);
    }

    /// Queues keys behind whatever the user has typed so far.
    pub fn input(&self, keys: &str) {
        self.request("nvim_input", vec![keys.into()]);
    }
}

/// Which window has Neovim's focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The home window: the notebook, navigated by nbv.
    Home,
    /// A cell's editor window.
    Cell(CellKey),
    /// Any other window: a help split, a picker, a prompt.
    Other,
}

/// The home window's position and size: the area the notebook is drawn in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
}

/// Colours of nbv's highlight groups, resolved by Neovim so they follow the colorscheme.
pub type Theme = HashMap<String, (Option<u32>, Option<u32>)>;

/// What the application needs to react to.
#[derive(Debug, PartialEq)]
pub enum EditorEvent {
    /// The grid is consistent and should be drawn.
    Flush,
    /// Startup (including the user's config) has finished.
    Ready,
    /// A cell's source changed through its buffer.
    SourceChanged(CellKey),
    /// The document was reloaded from disk.
    Reloaded,
    /// The document was written.
    Written,
    /// Neovim's focus moved, as of the application's focus request `seq`.
    Focus {
        seq: u64,
        focus: Focus,
    },
    /// A layout request has been applied and drawn by Neovim.
    LayoutDone {
        seq: u64,
        error: Option<String>,
    },
    Viewport(Viewport),
    Theme(Theme),
    /// A companion notification: a command or keymap the user invoked.
    Command {
        action: String,
        args: Value,
    },
    Exited,
}

/// Where a cell's editor window goes, in screen cells. `topline` 0 leaves the window's scroll
/// position to Neovim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorRect {
    pub key: CellKey,
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
    pub topline: usize,
}

/// A cell buffer as Rust mirrors it.
struct CellBuffer {
    key: CellKey,
    kind: CellKind,
    lines: Vec<String>,
}

pub struct Editor {
    pub grid: Grid,
    pub calls: Calls,
    home: Option<i64>,
    buffers: HashMap<i64, CellBuffer>,
    by_key: HashMap<CellKey, i64>,
    /// Buffers requested from the companion and not yet reported, with the type requested.
    creating: HashMap<CellKey, CellKind>,
    /// The kernel's language, which code cells are edited in.
    language: Option<String>,
}

pub fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

fn source_lines(source: &str) -> Vec<String> {
    source.split('\n').map(str::to_string).collect()
}

fn u16_field(v: &Value, key: &str) -> u16 {
    field(v, key).and_then(Value::as_u64).unwrap_or(0).min(u16::MAX as u64) as u16
}

fn viewport(v: &Value) -> Option<Viewport> {
    v.as_map()?;
    Some(Viewport {
        row: u16_field(v, "row"),
        col: u16_field(v, "col"),
        width: u16_field(v, "width"),
        height: u16_field(v, "height"),
    })
}

fn theme(v: &Value) -> Theme {
    let color = |c: &Value, k: &str| field(c, k).and_then(Value::as_i64).map(|n| n as u32);
    v.as_map()
        .into_iter()
        .flatten()
        .filter_map(|(k, c)| Some((k.as_str()?.to_string(), (color(c, "fg"), color(c, "bg")))))
        .collect()
}

impl Editor {
    /// The editor for a spawned Neovim. Run `handshake` concurrently with the event loop.
    pub fn new<C: NvimClient>(client: C, errors: mpsc::UnboundedSender<NvimError>) -> Editor {
        Editor {
            grid: Grid::default(),
            calls: Calls::start(client, errors),
            home: None,
            buffers: HashMap::new(),
            by_key: HashMap::new(),
            creating: HashMap::new(),
            language: None,
        }
    }

    /// Sets the language code cells are edited in. Changing it wipes every cell buffer; the
    /// next layout recreates them with the new filetype.
    pub fn set_language(&mut self, language: Option<String>) {
        if language != self.language {
            self.language = language;
            self.wipe_all();
        }
    }

    pub fn home(&self) -> Option<i64> {
        self.home
    }

    /// The buffer editing `key`, once the companion has created it.
    pub fn buffer(&self, key: &CellKey) -> Option<i64> {
        self.by_key.get(key).copied()
    }

    /// Places the cell editor windows (§11.1). Buffers are created for cells that have none;
    /// windows for cells not listed are closed. `active` is the cell being edited.
    pub fn layout(&mut self, nb: &Notebook, seq: u64, active: Option<&CellKey>, rects: &[EditorRect]) {
        let mut cells = vec![];
        for r in rects {
            let mut entries = vec![
                ("key", r.key.as_str().into()),
                ("row", (r.row as u64).into()),
                ("col", (r.col as u64).into()),
                ("width", (r.width as u64).into()),
                ("height", (r.height as u64).into()),
                ("topline", (r.topline as u64).into()),
            ];
            if !self.by_key.contains_key(&r.key)
                && !self.creating.contains_key(&r.key)
                && let Some(cell) = nb.cell(&r.key)
            {
                let kind = cell.kind();
                self.creating.insert(r.key.clone(), kind);
                let lines = source_lines(&cell.source()).into_iter().map(Value::from).collect();
                entries.push((
                    "create",
                    map(vec![
                        (
                            "name",
                            cell_buffer_name(nb, &r.key, kind, self.language.as_deref())
                                .to_string_lossy()
                                .into_owned()
                                .into(),
                        ),
                        ("filetype", filetype(kind, self.language.as_deref()).into()),
                        ("lines", Value::Array(lines)),
                    ]),
                ));
            }
            cells.push(map(entries));
        }
        let spec =
            map(vec![("active", active.map_or(Value::Nil, |k| k.as_str().into())), ("cells", Value::Array(cells))]);
        self.calls.lua("require('nbv').layout(...)", vec![seq.into(), spec]);
    }

    /// Makes the buffers agree with the document after a structural change: buffers of cells
    /// that left the document, or changed type, are wiped (a later layout recreates them), and
    /// buffers whose text differs from their cell's source are rewritten.
    pub fn sync(&mut self, nb: &Notebook) {
        let mut wipe = vec![];
        for (buf, b) in &mut self.buffers {
            let Some(cell) = nb.cell(&b.key).filter(|_| nb.is_live(&b.key)) else {
                wipe.push(*buf);
                continue;
            };
            if cell.kind() != b.kind {
                wipe.push(*buf);
                continue;
            }
            let lines = source_lines(&cell.source());
            if lines != b.lines {
                b.lines = lines.clone();
                let lines = Value::Array(lines.into_iter().map(Value::from).collect());
                self.calls.lua("require('nbv').set_text(...)", vec![(*buf).into(), lines]);
            }
        }
        self.wipe(wipe);
        self.creating.retain(|k, _| nb.is_live(k));
    }

    /// Wipes every cell buffer, e.g. after a reload.
    pub fn wipe_all(&mut self) {
        let all = self.buffers.keys().copied().collect();
        self.wipe(all);
        self.creating.clear();
    }

    fn wipe(&mut self, bufs: Vec<i64>) {
        if bufs.is_empty() {
            return;
        }
        for buf in &bufs {
            if let Some(b) = self.buffers.remove(buf) {
                self.by_key.remove(&b.key);
            }
        }
        let bufs = Value::Array(bufs.into_iter().map(Value::from).collect());
        self.calls.lua("require('nbv').wipe(...)", vec![bufs]);
    }

    /// Focuses a cell's editor window, optionally in insert mode.
    pub fn enter(&self, seq: u64, key: &CellKey, insert: bool) {
        self.calls.lua("require('nbv').enter(...)", vec![seq.into(), key.as_str().into(), insert.into()]);
    }

    /// Returns focus to the home window. Queued as input, behind keys already typed.
    pub fn leave(&self, seq: u64) {
        self.calls.input(&format!("<C-\\><C-n><Cmd>lua require('nbv').leave({seq})<CR>"));
    }

    /// Marks the home buffer modified, so quitting with unsaved changes is refused as usual.
    pub fn set_modified(&self) {
        self.calls.lua("require('nbv').set_modified()", vec![]);
    }

    /// Routes one Neovim event.
    pub fn handle(&mut self, nb: &mut Notebook, ev: NvimEvent) -> Vec<EditorEvent> {
        match ev {
            NvimEvent::Redraw(events) => {
                let mut flushed = false;
                for e in events {
                    flushed |= self.grid.apply(e);
                }
                if flushed { vec![EditorEvent::Flush] } else { vec![] }
            }
            NvimEvent::BufLines { buf, first, last, lines, .. } => {
                let Some(b) = self.buffers.get_mut(&buf) else { return vec![] };
                if last < 0 {
                    b.lines = lines;
                } else {
                    let (first, last) = (first as usize, last as usize);
                    if first > last || last > b.lines.len() {
                        // The mirror lost track of the buffer: resynchronise, always safe.
                        self.reattach(buf);
                        return vec![];
                    }
                    b.lines.splice(first..last, lines);
                }
                let key = b.key.clone();
                if nb.set_source(&key, &b.lines.join("\n")) { vec![EditorEvent::SourceChanged(key)] } else { vec![] }
            }
            NvimEvent::BufChangedTick { .. } => vec![],
            NvimEvent::BufDetach { buf } => {
                // Wiped buffers were forgotten already; anything else detached unexpectedly.
                if self.buffers.contains_key(&buf) {
                    self.reattach(buf);
                }
                vec![]
            }
            NvimEvent::Request { name, args, reply } => {
                let (value, events) = match name.as_str() {
                    "nbv_commit" => self.commit(nb, &args),
                    "nbv_reload" => self.reload(nb),
                    _ => (map(vec![("error", format!("unknown request {name}").into())]), vec![]),
                };
                let _ = reply.send(value);
                events
            }
            NvimEvent::Notify { name, args } if name == "nbv" => {
                let action = args.first().and_then(Value::as_str).unwrap_or("").to_string();
                let payload = args.get(1).cloned().unwrap_or(Value::Nil);
                self.notification(nb, action, payload)
            }
            NvimEvent::Notify { .. } => vec![],
            NvimEvent::Exited => vec![EditorEvent::Exited],
        }
    }

    fn notification(&mut self, nb: &Notebook, action: String, payload: Value) -> Vec<EditorEvent> {
        let seq = || field(&payload, "seq").and_then(Value::as_u64).unwrap_or(0);
        match action.as_str() {
            "ready" => {
                self.home = field(&payload, "home").and_then(Value::as_i64);
                let mut out = vec![];
                if let Some(v) = field(&payload, "viewport").and_then(viewport) {
                    out.push(EditorEvent::Viewport(v));
                }
                out.push(EditorEvent::Theme(field(&payload, "theme").map(theme).unwrap_or_default()));
                out.push(EditorEvent::Ready);
                out
            }
            "buffer" => {
                let key = field(&payload, "key").and_then(Value::as_str).map(CellKey::new);
                let buf = field(&payload, "buf").and_then(Value::as_i64);
                let (Some(key), Some(buf)) = (key, buf) else { return vec![] };
                let requested = self.creating.remove(&key);
                match (requested, nb.cell(&key).filter(|_| nb.is_live(&key))) {
                    (Some(kind), Some(cell)) if cell.kind() == kind => {
                        self.buffers.insert(buf, CellBuffer { key: key.clone(), kind, lines: vec![] });
                        self.by_key.insert(key, buf);
                        // The first event carries every line, including edits made meanwhile.
                        self.calls.request("nvim_buf_attach", vec![buf.into(), true.into(), Value::Map(vec![])]);
                    }
                    // The cell left, or changed type, while its buffer was being made.
                    _ => self.calls.lua("require('nbv').wipe(...)", vec![Value::Array(vec![buf.into()])]),
                }
                vec![]
            }
            "focus" => {
                let key = field(&payload, "key").and_then(Value::as_str).map(CellKey::new);
                let home = field(&payload, "home").and_then(Value::as_bool).unwrap_or(false);
                let focus = match key {
                    Some(k) if nb.is_live(&k) => Focus::Cell(k),
                    _ if home => Focus::Home,
                    _ => Focus::Other,
                };
                vec![EditorEvent::Focus { seq: seq(), focus }]
            }
            "layout_done" => {
                let error = field(&payload, "error").and_then(Value::as_str).map(str::to_string);
                let mut out = vec![EditorEvent::LayoutDone { seq: seq(), error }];
                if let Some(v) = field(&payload, "viewport").and_then(viewport) {
                    out.push(EditorEvent::Viewport(v));
                }
                out
            }
            "viewport" => viewport(&payload).map(EditorEvent::Viewport).into_iter().collect(),
            "theme" => vec![EditorEvent::Theme(theme(&payload))],
            _ => vec![EditorEvent::Command { action, args: payload }],
        }
    }

    /// Subscribes afresh with the whole buffer: the first event is a full resync.
    fn reattach(&self, buf: i64) {
        self.calls.request("nvim_buf_detach", vec![buf.into()]);
        self.calls.request("nvim_buf_attach", vec![buf.into(), true.into(), Value::Map(vec![])]);
    }

    /// `nbv_commit(force, buffers)` (§9.2). Neovim delivers buffer events before the request,
    /// so the mirrors are current; the companion sends every cell buffer's text anyway, and a
    /// mirror that differs is repaired before committing.
    fn commit(&mut self, nb: &mut Notebook, args: &[Value]) -> (Value, Vec<EditorEvent>) {
        let force = args.first().and_then(Value::as_bool).unwrap_or(false);
        for pair in args.get(1).and_then(Value::as_array).into_iter().flatten() {
            let pair = pair.as_array().map(Vec::as_slice).unwrap_or(&[]);
            let (Some(buf), Some(lines)) = (pair.first().and_then(Value::as_i64), pair.get(1)) else { continue };
            if let Some(b) = self.buffers.get_mut(&buf) {
                b.lines = strings(lines);
                nb.set_source(&b.key, &b.lines.join("\n"));
            }
        }
        match nb.commit(CommitOptions { force }) {
            Ok(()) => {
                let name = nb.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let message = format!("\"{name}\" {}C written", nb.order().len());
                (map(vec![("message", message.into())]), vec![EditorEvent::Written])
            }
            Err(e) => (map(vec![("error", e.to_string().into())]), vec![]),
        }
    }

    /// `nbv_reload` (§9.5): `:e!` reloads the notebook from disk.
    fn reload(&mut self, nb: &mut Notebook) -> (Value, Vec<EditorEvent>) {
        match nb.reload() {
            Ok(()) => {
                // Queued: Neovim is inside the read request until the reply arrives.
                self.wipe_all();
                (map(vec![]), vec![EditorEvent::Reloaded])
            }
            Err(e) => (map(vec![("error", e.to_string().into())]), vec![]),
        }
    }
}

/// Spawns Neovim for a notebook. `extra` goes before the file argument (tests use `-u`).
pub async fn spawn(
    program: PathBuf,
    clean: bool,
    extra: &[String],
    notebook: &Path,
) -> Result<(crate::client::EmbeddedNvim, mpsc::UnboundedReceiver<NvimEvent>), NvimError> {
    let mut args = extra.to_vec();
    args.extend(["--".into(), home_name(notebook)]);
    crate::client::EmbeddedNvim::spawn(SpawnOptions { program, clean, args }).await
}

/// Loads the companion before the user's config (pre-config, §10.3), then attaches the UI,
/// which lets startup proceed: user config, the home buffer, then `VimEnter`.
pub async fn handshake<C: NvimClient>(
    client: &C,
    notebook: &Path,
    width: usize,
    height: usize,
) -> Result<(), NvimError> {
    let info = client.request("nvim_get_api_info", vec![]).await?;
    let chan = info.as_array().and_then(|a| a.first()).and_then(Value::as_i64).unwrap_or(1);
    client
        .exec_lua(
            "local src, chan, home, sp = ...\n\
             local M = assert(loadstring(src, '@nbv/init.lua'))()\n\
             M.pre(chan, home, sp)",
            vec![
                COMPANION.into(),
                chan.into(),
                home_name(notebook).into(),
                (crate::grid::TRANSPARENT_SP as u64).into(),
            ],
        )
        .await?;
    client.attach_ui(width, height).await
}
