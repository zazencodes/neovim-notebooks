//! The notebook buffer adapter (core ↔ Neovim buffer) and the editor driver that routes
//! Neovim's events: redraws to the grid, buffer events to reconciliation, the companion's
//! reads and writes to persistence (§9), and everything else to the application.

use std::path::{Path, PathBuf};

use nbv_core::{CommitOptions, LanguageProjection, LineEdit, Notebook};
use rmpv::Value;
use tokio::sync::mpsc;

use crate::client::{NvimClient, NvimError, NvimEvent, SpawnOptions, strings};
use crate::grid::Grid;

/// The companion (R6), embedded in the binary and loaded pre-config.
pub const COMPANION: &str = include_str!("../../../lua/nbv/init.lua");

/// The projected buffer's name: the notebook path plus the adapter suffix (§9.1).
pub fn buffer_path(notebook: &Path, nb: &Notebook) -> PathBuf {
    let mut s = notebook.as_os_str().to_owned();
    s.push(nb.adapter().buffer_suffix());
    PathBuf::from(s)
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
}

/// What the application needs to react to.
#[derive(Debug, PartialEq)]
pub enum EditorEvent {
    /// The grid is consistent and should be drawn.
    Flush,
    /// Startup (including the user's config) has finished.
    Ready,
    /// The notebook document changed through the buffer.
    DocumentChanged,
    /// The document was reloaded from disk.
    Reloaded,
    /// A companion notification: a command or keymap the user invoked.
    Command {
        action: String,
        args: Value,
    },
    Exited,
}

pub struct Editor {
    pub grid: Grid,
    pub calls: Calls,
    buf: Option<i64>,
    attached: bool,
    /// Detach events our own reattachments will provoke, to be ignored.
    expected_detaches: usize,
    /// changedtick of the last buffer event reconciled.
    tick: u64,
    loaded_once: bool,
}

fn edits_value(edits: &[LineEdit]) -> Value {
    Value::Array(
        edits
            .iter()
            .map(|e| {
                Value::Array(vec![
                    (e.first as u64).into(),
                    (e.last as u64).into(),
                    Value::Array(e.lines.iter().map(|l| l.as_str().into()).collect()),
                ])
            })
            .collect(),
    )
}

pub fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

fn reply_map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

impl Editor {
    /// The editor for a spawned Neovim. Run `handshake` concurrently with the event loop:
    /// startup reads the notebook through `nbv_load`, which only the loop can answer.
    pub fn new<C: NvimClient>(client: C, errors: mpsc::UnboundedSender<NvimError>) -> Editor {
        Editor {
            grid: Grid::default(),
            calls: Calls::start(client, errors),
            buf: None,
            attached: false,
            expected_detaches: 0,
            tick: 0,
            loaded_once: false,
        }
    }

    pub fn buffer(&self) -> Option<i64> {
        self.buf
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Applies edits to the notebook buffer if it is still at `tick` (§7.3). `join` makes them
    /// part of the user's last change and defers them in insert mode.
    pub fn apply(&self, tick: u64, edits: &[LineEdit], join: bool, cursor: Option<usize>) {
        let Some(buf) = self.buf else { return };
        if edits.is_empty() && cursor.is_none() {
            return;
        }
        self.calls.lua(
            "return require('nbv').apply(...)",
            vec![
                buf.into(),
                tick.into(),
                edits_value(edits),
                join.into(),
                cursor.map_or(Value::Nil, |c| (c as u64).into()),
            ],
        );
    }

    fn normalise(&self, nb: &Notebook) {
        self.apply(self.tick, &nb.pending_normalisation(), true, None);
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
            NvimEvent::BufLines { buf, tick, first, last, lines } => {
                if Some(buf) != self.buf || !self.attached {
                    return vec![];
                }
                if let Some(t) = tick {
                    self.tick = t;
                }
                let reconciled = if last < 0 {
                    Ok(nb.resync(lines))
                } else {
                    nb.apply_edit(LineEdit { first: first as usize, last: last as usize, lines })
                };
                match reconciled {
                    Ok(r) => {
                        if !r.normalise.is_empty() {
                            self.apply(self.tick, &r.normalise, true, None);
                        }
                        if r.changed { vec![EditorEvent::DocumentChanged] } else { vec![] }
                    }
                    Err(_) => {
                        // The mirror lost track of the buffer: resynchronise, always safe.
                        self.reattach();
                        vec![]
                    }
                }
            }
            NvimEvent::BufChangedTick { buf, tick } => {
                if Some(buf) == self.buf && self.attached {
                    self.tick = tick;
                }
                vec![]
            }
            NvimEvent::BufDetach { buf } => {
                if Some(buf) == self.buf {
                    if self.expected_detaches > 0 {
                        self.expected_detaches -= 1;
                    } else {
                        // Reloads (:e!) detach; the companion's `loaded` notice reattaches.
                        self.attached = false;
                    }
                }
                vec![]
            }
            NvimEvent::Request { name, args, reply } => {
                let (value, events) = match name.as_str() {
                    "nbv_load" => self.load(nb, &args),
                    "nbv_commit" => (self.commit(nb, &args), vec![]),
                    _ => (reply_map(vec![("error", format!("unknown request {name}").into())]), vec![]),
                };
                let _ = reply.send(value);
                events
            }
            NvimEvent::Notify { name, args } if name == "nbv" => {
                let action = args.first().and_then(Value::as_str).unwrap_or("").to_string();
                let payload = args.get(1).cloned().unwrap_or(Value::Nil);
                match action.as_str() {
                    "ready" => {
                        if let Some(b) = field(&payload, "buf").and_then(Value::as_i64).filter(|b| *b > 0) {
                            self.buf = Some(b);
                        }
                        vec![EditorEvent::Ready]
                    }
                    "loaded" => {
                        self.buf = field(&payload, "buf").and_then(Value::as_i64).or(self.buf);
                        self.reattach();
                        vec![]
                    }
                    "normalise" => {
                        self.normalise(nb);
                        vec![]
                    }
                    _ => vec![EditorEvent::Command { action, args: payload }],
                }
            }
            NvimEvent::Notify { .. } => vec![],
            NvimEvent::Exited => vec![EditorEvent::Exited],
        }
    }

    /// Subscribes afresh with the whole buffer: the first event is a full resync.
    fn reattach(&mut self) {
        let Some(buf) = self.buf else { return };
        if self.attached {
            self.expected_detaches += 1;
            self.calls.request("nvim_buf_detach", vec![buf.into()]);
        }
        self.attached = true;
        self.calls.request("nvim_buf_attach", vec![buf.into(), true.into(), Value::Map(vec![])]);
    }

    /// `nbv_load`: the first read projects the document; later reads (:e, :e!) reload it from
    /// disk (§9.5).
    fn load(&mut self, nb: &mut Notebook, args: &[Value]) -> (Value, Vec<EditorEvent>) {
        self.buf = args.first().and_then(Value::as_i64).or(self.buf);
        let filetype = nb.adapter().filetype().to_string();
        let (lines, events) = if !self.loaded_once {
            self.loaded_once = true;
            (nb.mirror().to_vec(), vec![])
        } else {
            match nb.reload() {
                Ok(lines) => (lines, vec![EditorEvent::Reloaded]),
                Err(e) => return (reply_map(vec![("error", e.to_string().into())]), vec![]),
            }
        };
        let lines = Value::Array(lines.into_iter().map(Value::from).collect());
        (reply_map(vec![("lines", lines), ("filetype", filetype.into())]), events)
    }

    /// `nbv_commit { changedtick, force, lines }` (§9.2). The companion sends the buffer text
    /// too: if it differs from the mirror, a resync repairs it before committing.
    fn commit(&mut self, nb: &mut Notebook, args: &[Value]) -> Value {
        let force = args.get(1).and_then(Value::as_bool).unwrap_or(false);
        let lines = args.get(2).map(strings).unwrap_or_default();
        if lines != nb.mirror() {
            let r = nb.resync(lines);
            if !r.normalise.is_empty() {
                let tick = args.first().and_then(Value::as_u64).unwrap_or(self.tick);
                self.apply(tick, &r.normalise, true, None);
            }
        }
        match nb.commit(CommitOptions { force }) {
            Ok(()) => {
                let name = nb.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let message = format!("\"{name}\" {}C written", nb.order().len());
                reply_map(vec![("message", message.into())])
            }
            Err(e) => reply_map(vec![("error", e.to_string().into())]),
        }
    }
}

/// Spawns Neovim for a notebook. `extra` goes before the file argument (tests use `-u`).
pub async fn spawn(
    program: PathBuf,
    clean: bool,
    extra: &[String],
    buffer_path: &Path,
) -> Result<(crate::client::EmbeddedNvim, mpsc::UnboundedReceiver<NvimEvent>), NvimError> {
    let mut args = extra.to_vec();
    args.extend(["--".into(), buffer_path.to_string_lossy().into_owned()]);
    crate::client::EmbeddedNvim::spawn(SpawnOptions { program, clean, args }).await
}

/// Loads the companion before the user's config (pre-config, §10.3), then attaches the UI,
/// which lets startup proceed: user config, then the notebook buffer via `BufReadCmd`.
pub async fn handshake<C: NvimClient>(
    client: &C,
    buffer_path: &Path,
    width: usize,
    height: usize,
) -> Result<(), NvimError> {
    let info = client.request("nvim_get_api_info", vec![]).await?;
    let chan = info.as_array().and_then(|a| a.first()).and_then(Value::as_i64).unwrap_or(1);
    let path = buffer_path.to_string_lossy().into_owned();
    client
        .exec_lua(
            "local src, chan, path = ...\n\
             local M = assert(loadstring(src, '@nbv/init.lua'))()\n\
             M.pre(chan, path)",
            vec![COMPANION.into(), chan.into(), path.into()],
        )
        .await?;
    client.attach_ui(width, height).await
}
