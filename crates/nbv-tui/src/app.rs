//! The event loop: terminal input, Neovim, and the kernel, around one notebook (R1).
//!
//! nbv draws the notebook: cell boxes, execution counts, outputs. Neovim draws each visible
//! cell's editor in a floating window placed inside its box, and everything else Neovim shows
//! (command line, pickers, completion) on top. Keys go to nbv's navigation mode while the
//! home window has focus, and to Neovim while a cell (or anything else) does.

use std::collections::{HashMap, VecDeque};
use std::io::{Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use crossterm::{cursor, event, execute, terminal};
use futures::StreamExt;
use jupyter_protocol::JupyterMessage;
use nbv_core::exec::{ExecEvent, Executor, KernelStatus};
use nbv_core::kernel::{self, Kernel, KernelCommand, KernelError, KernelMessage};
use nbv_core::memory::KernelMemory;
use nbv_core::{CellKey, CellKind, Change, ExecState, Notebook};
use nbv_nvim::editor::{self, Editor, EditorEvent, EditorRect, Focus, Theme, field};
use nbv_nvim::redraw::CursorShape;
use nbv_nvim::{EmbeddedNvim, NvimClient, NvimError, NvimEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Size;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_image::picker::Picker;
use ratatui_image::sliced::SlicedProtocol;
use rmpv::Value;
use serde_json::Map;
use tokio::sync::mpsc;

use crate::compose::{self, Region};
use crate::layout::{Block, Geometry, Layout};
use crate::nav::{self, Action, Nav};
use crate::outputs::{self, Images, OutputView};
use crate::terminal::{self as term, Tmux};

pub struct Options {
    pub notebook: PathBuf,
    pub clean: bool,
    pub nvim: PathBuf,
}

/// Where a failed or missing kernel's message points.
const PICK_KERNEL: &str = "pick one in the header: gg k l <CR>";

enum KernelStart {
    Ready(u64, Box<Result<Kernel, KernelError>>),
    /// The kernel's Python lacks ipykernel; the command installs it.
    MissingIpykernel(u64, Vec<String>),
    Installed(u64, Result<(), KernelError>),
}

/// Where keys go.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// nbv's navigation keys act on cells.
    Nav,
    /// A cell's Neovim window has focus; keys go to Neovim.
    Edit(CellKey),
    /// Some other Neovim window has focus; keys go to Neovim.
    Other,
}

/// What can be selected in the header, above the first cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderItem {
    /// The notebook's file name: `<CR>` renames the file.
    Name,
    /// The kernel: `<CR>` picks another.
    Kernel,
}

struct App {
    nb: Notebook,
    editor: Editor,
    client: EmbeddedNvim,
    exec: Executor,
    kernel: Option<Kernel>,
    /// The kernel chosen for this session: resolved at startup, or picked in the header.
    kernel_cmd: Option<KernelCommand>,
    /// Why there is no working kernel. Runs fail with it, shown under the cell.
    kernel_error: Option<String>,
    /// What the kernel picker last offered, in the order shown.
    kernel_choices: Vec<KernelCommand>,
    /// The ipykernel install offered for a kernel start, and that start's generation.
    install: Option<(u64, Vec<String>)>,
    /// Whether Neovim has finished starting, so prompts can be shown.
    nvim_ready: bool,
    generation: u64,
    kernel_tx: mpsc::UnboundedSender<KernelMessage>,
    start_tx: mpsc::UnboundedSender<KernelStart>,
    /// Requests made before the kernel was ready.
    pending: Vec<JupyterMessage>,
    input_request: Option<JupyterMessage>,
    message: Option<String>,
    /// tmux passes on no modified keys, so `<S-CR>` and `<C-CR>` arrive as `<CR>`.
    tmux_keys_missing: bool,
    /// The kernel picked for each notebook, across sessions.
    memory: KernelMemory,
    views: HashMap<CellKey, OutputView>,
    images: Images,
    protocols: HashMap<(u64, u16, u16), SlicedProtocol>,
    picker: Picker,
    tmux: Option<Tmux>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
    cursor_shape: Option<(CursorShape, u8)>,
    quit: bool,

    mode: Mode,
    /// Sequence number of the latest focus change nbv asked for (§10.2).
    focus_seq: u64,
    selected: usize,
    /// The header item selected in navigation mode, instead of a cell.
    header: Option<HeaderItem>,
    scroll: usize,
    geometry: Option<Geometry>,
    theme: Theme,
    nav: Nav,
    /// The last deleted or yanked cell, for pasting.
    register: Option<Map<String, serde_json::Value>>,
    /// Keys go to Neovim until it returns to Normal mode after this mode-change count.
    forward_since: Option<u64>,
    /// Whether the next layout should scroll the selected cell into view.
    reveal: bool,
    layout_seq: u64,
    /// The last editor placement sent, to skip identical ones.
    last_sent: Option<(Option<CellKey>, Vec<EditorRect>)>,
    /// Layouts sent and not yet on screen, with their scroll offsets.
    sent: VecDeque<(u64, Layout, usize)>,
    /// The layout Neovim has on screen: nbv draws around the windows where they are.
    shown: Option<(Layout, usize)>,
    relayout: bool,
    draw: bool,
    /// Whether the home buffer has been marked modified since the last write.
    dirty: bool,
}

/// Restores the terminal on drop, including on panic.
struct TerminalGuard {
    tmux: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        term::stop_reporting_modified_keys(self.tmux);
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            event::DisableMouseCapture,
            event::DisableBracketedPaste,
            event::DisableFocusChange,
            cursor::SetCursorStyle::DefaultUserShape,
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let notebook = std::path::absolute(&opts.notebook)?;
    let nb = Notebook::open(&notebook)?;
    let tmux = term::in_tmux().then(term::probe_tmux);

    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(
        out,
        terminal::EnterAlternateScreen,
        event::EnableMouseCapture,
        event::EnableBracketedPaste,
        event::EnableFocusChange
    )?;
    let _guard = TerminalGuard { tmux: tmux.is_some() };
    // The capability query reads stdin, so it must run before the event stream starts.
    let picker = term::picker(tmux.as_ref());
    term::report_modified_keys(tmux.is_some())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    let size = terminal.size()?;

    // Before Neovim starts: code cells are edited in the kernel's language.
    // The kernel picked last time comes first; without one, nbv picks as Jupyter does.
    let memory = KernelMemory::open()?;
    let resolved = match memory.get(nb.path()) {
        Some(cmd) => Ok(cmd.clone()),
        None => kernel::resolve(nb.kernel_name()).await,
    };
    let (kernel_cmd, kernel_error) = match resolved {
        Ok(cmd) => (Some(cmd), None),
        Err(e) => (None, Some(format!("{e} ({PICK_KERNEL})"))),
    };
    let (client, mut nvim_rx) = editor::spawn(opts.nvim.clone(), opts.clean, &[], &notebook).await?;
    let (err_tx, mut err_rx) = mpsc::unbounded_channel::<NvimError>();
    let mut editor = Editor::new(client.clone(), err_tx.clone());
    editor.set_language(kernel_cmd.as_ref().map(|c| c.language.clone()));
    {
        let (client, path) = (client.clone(), notebook.clone());
        let (w, h) = (size.width as usize, size.height as usize);
        tokio::spawn(async move {
            if let Err(e) = editor::handshake(&client, &path, w, h).await {
                let _ = err_tx.send(e);
            }
        });
    }

    let (kernel_tx, mut kernel_rx) = mpsc::unbounded_channel();
    let (start_tx, mut start_rx) = mpsc::unbounded_channel();
    let mut app = App {
        nb,
        editor,
        client,
        exec: Executor::default(),
        kernel: None,
        kernel_cmd,
        kernel_error,
        kernel_choices: vec![],
        install: None,
        nvim_ready: false,
        generation: 0,
        kernel_tx,
        start_tx,
        pending: vec![],
        input_request: None,
        message: None,
        // Short enough not to wrap into a hit-enter prompt; the README has the lines to add.
        tmux_keys_missing: tmux.as_ref().is_some_and(|t| !t.extended_keys),
        memory,
        views: HashMap::new(),
        images: Images::default(),
        protocols: HashMap::new(),
        picker,
        tmux,
        terminal,
        cursor_shape: None,
        quit: false,
        mode: Mode::Nav,
        focus_seq: 0,
        selected: 0,
        header: None,
        scroll: 0,
        geometry: None,
        theme: Theme::new(),
        nav: Nav::default(),
        register: None,
        forward_since: None,
        reveal: false,
        layout_seq: 0,
        last_sent: None,
        sent: VecDeque::new(),
        shown: None,
        relayout: false,
        draw: true,
        dirty: false,
    };
    app.start_kernel();

    let mut events = EventStream::new();
    while !app.quit {
        tokio::select! {
            ev = nvim_rx.recv() => match ev {
                Some(ev) => app.on_nvim(ev).await,
                None => app.quit = true,
            },
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.on_terminal(ev).await,
                _ => app.quit = true,
            },
            Some(msg) = kernel_rx.recv() => app.on_kernel(msg).await,
            Some(start) = start_rx.recv() => app.on_kernel_start(start).await,
            Some(e) = err_rx.recv() => {
                app.message = Some(e.to_string());
                app.draw = true;
            }
        }
        // Coalesce whatever else is already waiting before drawing.
        while let Ok(ev) = nvim_rx.try_recv() {
            app.on_nvim(ev).await;
        }
        while let Ok(msg) = kernel_rx.try_recv() {
            app.on_kernel(msg).await;
        }
        if app.relayout {
            app.relayout();
        }
        if app.draw {
            app.draw()?;
        }
    }

    if let Some(k) = app.kernel.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), k.shutdown()).await;
    }
    let _ = tokio::time::timeout(Duration::from_millis(500), app.client.quit()).await;
    Ok(())
}

fn rgb(c: Option<u32>) -> Option<Color> {
    c.map(|c| Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8))
}

impl App {
    /// Starts the chosen kernel. Without one, `kernel_error` already says why.
    fn start_kernel(&mut self) {
        self.generation += 1;
        let Some(cmd) = self.kernel_cmd.clone() else {
            let reason = self.kernel_error.clone().expect("no kernel without a reason");
            for ev in self.exec.fail_all(&mut self.nb, &reason) {
                self.on_exec_event(ev);
            }
            return;
        };
        self.kernel_error = None;
        self.install = None;
        let generation = self.generation;
        let cwd = self.nb.path().parent().expect("the notebook path is absolute").to_path_buf();
        let (tx, start) = (self.kernel_tx.clone(), self.start_tx.clone());
        tokio::spawn(async move {
            let result = match kernel::ipykernel_install(&cmd).await {
                Ok(Some(install)) => {
                    let _ = start.send(KernelStart::MissingIpykernel(generation, install));
                    return;
                }
                Ok(None) => Kernel::start(&cmd, &cwd, generation, tx).await,
                Err(e) => Err(e),
            };
            let _ = start.send(KernelStart::Ready(generation, Box::new(result)));
        });
    }

    fn kernel_name(&self) -> &str {
        self.kernel_cmd.as_ref().map_or("kernel", |c| c.display_name.as_str())
    }

    async fn on_kernel_start(&mut self, start: KernelStart) {
        match start {
            KernelStart::Ready(generation, result) => self.on_kernel_ready(generation, *result).await,
            KernelStart::MissingIpykernel(generation, install) if generation == self.generation => {
                self.install = Some((generation, install));
                self.offer_install();
            }
            KernelStart::Installed(generation, result) if generation == self.generation => {
                match result {
                    Ok(()) => {
                        self.message = Some(format!("installed ipykernel for {}", self.kernel_name()));
                        self.start_kernel();
                    }
                    Err(e) => self.kernel_failed(format!(
                        "{} could not start (00 retries; {PICK_KERNEL}):\n{e}",
                        self.kernel_name()
                    )),
                }
                self.draw = true;
            }
            KernelStart::MissingIpykernel(..) | KernelStart::Installed(..) => {}
        }
    }

    /// Asks whether to install ipykernel for the kernel being started, once Neovim can ask.
    fn offer_install(&mut self) {
        let Some((generation, install)) = self.install.as_ref().filter(|_| self.nvim_ready) else { return };
        let prompt = format!("{} has no ipykernel. Install it with `{}`?", self.kernel_name(), install.join(" "));
        self.editor.calls.lua("require('nbv').offer_install(...)", vec![prompt.into(), (*generation).into()]);
    }

    /// The user answered `offer_install`: install ipykernel, then start the kernel.
    fn install_answered(&mut self, generation: u64, yes: bool) {
        let Some((_, install)) = self.install.take_if(|(g, _)| *g == generation) else { return };
        if !yes {
            self.kernel_failed(format!(
                "{} has no ipykernel (00 offers to install it; {PICK_KERNEL})",
                self.kernel_name()
            ));
            self.draw = true;
            return;
        }
        self.message = Some(format!("installing ipykernel: {}", install.join(" ")));
        self.draw = true;
        let start = self.start_tx.clone();
        tokio::spawn(async move {
            let result = kernel::install(&install).await;
            let _ = start.send(KernelStart::Installed(generation, result));
        });
    }

    /// The kernel cannot run anything: cells queued or running fail with `reason`, and so will
    /// later runs until the kernel is restarted or another is picked.
    fn kernel_failed(&mut self, reason: String) {
        self.kernel = None;
        self.pending.clear();
        for ev in self.exec.fail_all(&mut self.nb, &reason) {
            self.on_exec_event(ev);
        }
        self.kernel_error = Some(reason);
    }

    async fn on_kernel_ready(&mut self, generation: u64, result: Result<Kernel, KernelError>) {
        if generation != self.generation {
            if let Ok(k) = result {
                tokio::spawn(k.shutdown());
            }
            return;
        }
        match result {
            Ok(mut k) => {
                for msg in self.pending.drain(..) {
                    if let Err(e) = k.send_shell(msg).await {
                        self.message = Some(e.to_string());
                    }
                }
                self.kernel = Some(k);
                self.exec.connected();
            }
            Err(e) => {
                self.kernel_failed(format!("{} could not start (00 retries; {PICK_KERNEL}):\n{e}", self.kernel_name()));
            }
        }
        self.draw = true;
    }

    async fn on_nvim(&mut self, ev: NvimEvent) {
        for e in self.editor.handle(&mut self.nb, ev) {
            match e {
                EditorEvent::Flush => self.draw = true,
                EditorEvent::Ready => {
                    self.nvim_ready = true;
                    self.offer_install();
                    self.relayout = true;
                }
                EditorEvent::SourceChanged(_) => {
                    // So `:x` in the home window writes it, as for any changed buffer.
                    self.mark_dirty();
                    // A cell's line count may have changed, and its staleness.
                    self.relayout = true;
                    self.draw = true;
                }
                EditorEvent::Rows => self.relayout = true,
                EditorEvent::Reloaded => {
                    self.views.clear();
                    self.mode = Mode::Nav;
                    self.dirty = false;
                    self.last_sent = None;
                    self.relayout = true;
                }
                EditorEvent::Written => {
                    self.dirty = false;
                    self.draw = true;
                }
                EditorEvent::Focus { seq, focus } => {
                    // Focus changes that predate nbv's latest request are superseded by it.
                    if seq >= self.focus_seq {
                        let mode = match focus {
                            Focus::Home => Mode::Nav,
                            Focus::Cell(k) => {
                                self.selected = self.nb.index_of(&k).unwrap_or(self.selected);
                                Mode::Edit(k)
                            }
                            Focus::Other => Mode::Other,
                        };
                        if mode != self.mode {
                            self.mode = mode;
                            self.relayout = true;
                        }
                    }
                }
                EditorEvent::LayoutDone { seq, error } => {
                    while let Some((s, ..)) = self.sent.front() {
                        if *s > seq {
                            break;
                        }
                        let (s, layout, scroll) = self.sent.pop_front().expect("front exists");
                        if s == seq {
                            self.shown = Some((layout, scroll));
                        }
                    }
                    if let Some(e) = error {
                        self.message = Some(format!("layout: {e}"));
                    }
                    self.draw = true;
                }
                EditorEvent::Viewport(v) => {
                    let g = Geometry { viewport: v };
                    if self.geometry != Some(g) {
                        self.geometry = Some(g);
                        self.relayout = true;
                    }
                }
                EditorEvent::Theme(t) => {
                    self.theme = t;
                    self.draw = true;
                }
                EditorEvent::Command { action, args } => self.on_command(&action, &args).await,
                EditorEvent::Exited => self.quit = true,
            }
        }
    }

    async fn on_terminal(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.on_key(k).await,
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(text) => {
                if self.mode != Mode::Nav || self.forwarding() {
                    self.editor.calls.request("nvim_paste", vec![text.into(), true.into(), (-1).into()]);
                }
            }
            Event::Resize(w, h) => {
                self.editor.calls.request("nvim_ui_try_resize", vec![(w as u64).into(), (h as u64).into()]);
                self.views.clear();
                self.protocols.clear();
                self.relayout = true;
            }
            Event::FocusGained => {
                self.editor.calls.request("nvim_ui_set_focus", vec![true.into()]);
                if self.tmux.is_some() {
                    // A reattached client may be a different terminal: resend image data.
                    self.protocols.clear();
                    let _ = self.terminal.clear();
                    self.draw = true;
                }
            }
            Event::FocusLost => self.editor.calls.request("nvim_ui_set_focus", vec![false.into()]),
        }
    }

    /// Whether keys typed in navigation mode belong to Neovim: its command line, a prompt,
    /// or a `:` command nbv handed over and Neovim has not finished.
    fn forwarding(&mut self) -> bool {
        let grid = &self.editor.grid;
        if let Some(since) = self.forward_since {
            if grid.mode_name == "normal" && grid.mode_changes >= since + 2 {
                self.forward_since = None;
            } else {
                return true;
            }
        }
        grid.mode_name != "normal"
    }

    async fn on_key(&mut self, k: KeyEvent) {
        let Some(keys) = crate::keys::encode(k) else { return };
        let calls = self.editor.calls.clone();
        match self.mode.clone() {
            // A cell's own <Esc> mapping leaves it (§10.2); everything else is Neovim's.
            Mode::Edit(_) | Mode::Other => calls.input(&keys),
            Mode::Nav if self.forwarding() => calls.input(&keys),
            Mode::Nav => match self.nav.feed(&keys) {
                Some(action) => self.on_action(action).await,
                None => self.draw = true,
            },
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let forward = |app: &App| {
            if let Some((button, action, modifier, row, col)) = crate::keys::mouse(m) {
                let args = vec![
                    button.into(),
                    action.into(),
                    modifier.into(),
                    0u64.into(),
                    (row as u64).into(),
                    (col as u64).into(),
                ];
                app.editor.calls.request("nvim_input_mouse", args);
            }
        };
        let (Some(g), Some((layout, scroll))) = (self.geometry, self.shown.as_ref()) else { return forward(self) };
        let scroll = *scroll;
        let hit = layout.at_row(&g, scroll, m.row).map(|i| layout.blocks[i].key.clone());
        let in_editor = hit.as_ref().is_some_and(|key| {
            layout.editors(&g, scroll, None).iter().any(|r| {
                &r.key == key
                    && (r.row..r.row + r.height).contains(&m.row)
                    && (r.col..r.col + r.width).contains(&m.column)
            })
        });
        match (m.kind, &self.mode) {
            (MouseEventKind::ScrollDown | MouseEventKind::ScrollUp, Mode::Nav) => {
                let rows = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                self.scroll_by(rows);
            }
            (MouseEventKind::Down(MouseButton::Left), mode) if hit.is_some() => {
                let key = hit.expect("checked");
                let editing = mode == &Mode::Edit(key.clone());
                if in_editor {
                    if !editing {
                        self.enter(key, false);
                    }
                    // Positions the cursor, now that the window has focus.
                    forward(self);
                } else {
                    if matches!(self.mode, Mode::Edit(_)) {
                        self.leave();
                    }
                    self.select(self.nb.index_of(&key).unwrap_or(self.selected));
                }
            }
            (_, Mode::Nav) if hit.is_some() => {}
            _ => forward(self),
        }
    }

    async fn on_kernel(&mut self, msg: KernelMessage) {
        match msg {
            KernelMessage::Message(m) => {
                if kernel::is_input_request(&m) {
                    self.input_request = Some((*m).clone());
                }
                for ev in self.exec.handle(&mut self.nb, &m) {
                    self.on_exec_event(ev);
                }
            }
            KernelMessage::Died { generation, stderr } if generation == self.generation => {
                let tail = stderr.trim_end();
                let tail = if tail.is_empty() { String::new() } else { format!(":\n{tail}") };
                self.kernel_failed(format!("the kernel died (00 restarts it){tail}"));
            }
            KernelMessage::Died { .. } => {}
        }
    }

    fn on_exec_event(&mut self, ev: ExecEvent) {
        match ev {
            ExecEvent::Cell(k) => {
                self.views.remove(&k);
                self.mark_dirty();
                self.relayout = true;
                self.draw = true;
            }
            ExecEvent::Status(_) => self.draw = true,
            ExecEvent::InputRequested { prompt, password, .. } => {
                self.editor.calls.lua("require('nbv').input(...)", vec![prompt.into(), password.into()]);
            }
        }
    }

    /// The document changed: mark the home buffer modified, so quitting without saving is
    /// refused, and `:x` writes, as for any buffer.
    fn mark_dirty(&mut self) {
        if !self.dirty {
            self.dirty = true;
            self.editor.set_modified();
        }
    }

    fn selected_key(&self) -> Option<CellKey> {
        self.nb.order().get(self.selected).cloned()
    }

    /// The cell a command names (the cell whose window ran it), or the selected cell.
    fn command_key(&self, args: &Value) -> Option<CellKey> {
        field(args, "key")
            .and_then(Value::as_str)
            .map(CellKey::new)
            .filter(|k| self.nb.is_live(k))
            .or_else(|| self.selected_key())
    }

    async fn run_cells(&mut self, keys: Vec<CellKey>) {
        for key in keys {
            if let Some(reason) = self.kernel_error.clone() {
                if self.nb.cell(&key).is_some_and(|c| c.kind() == CellKind::Code) {
                    let ev = self.exec.fail(&mut self.nb, &key, &reason);
                    self.on_exec_event(ev);
                }
                continue;
            }
            let Some(msg) = self.exec.request(&mut self.nb, &key) else { continue };
            self.views.remove(&key);
            match self.kernel.as_mut() {
                Some(k) => {
                    if let Err(e) = k.send_shell(msg).await {
                        self.message = Some(e.to_string());
                    }
                }
                None => self.pending.push(msg),
            }
        }
        self.relayout = true;
        self.draw = true;
    }

    fn code_cells(&self) -> Vec<CellKey> {
        let nb = &self.nb;
        nb.order().iter().filter(|k| nb.cell(k).is_some_and(|c| c.kind() == CellKind::Code)).cloned().collect()
    }

    async fn restart(&mut self) {
        if let Some(k) = self.kernel.take() {
            tokio::spawn(k.shutdown());
        }
        for ev in self.exec.reset(&mut self.nb, KernelStatus::Restarting) {
            self.on_exec_event(ev);
        }
        self.pending.clear();
        self.start_kernel();
    }

    async fn interrupt(&mut self) {
        if let Some(k) = self.kernel.as_mut()
            && let Err(e) = k.interrupt().await
        {
            self.message = Some(e.to_string());
        }
    }

    /// After running `key` with run-and-advance: select the next cell, or add one at the end
    /// and edit it, as Jupyter does.
    fn advance(&mut self, key: &CellKey) {
        let Some(i) = self.nb.index_of(key) else { return };
        if i + 1 < self.nb.order().len() {
            self.select(i + 1);
        } else {
            let key = self.add(i + 1);
            self.enter(key, true);
        }
    }

    /// Adds an empty code cell at `index` and selects it.
    fn add(&mut self, index: usize) -> CellKey {
        let key = self.nb.new_cell(CellKind::Code, "");
        let at = self.nb.edit(vec![Change::Show { key: key.clone(), index }]);
        self.structure_changed(at);
        key
    }

    /// Brings the buffers, the selection and the screen up to date after a structural change.
    fn structure_changed(&mut self, focus: Option<usize>) {
        self.editor.sync(&self.nb);
        if let Some(i) = focus {
            self.selected = i;
        }
        self.mark_dirty();
        self.last_sent = None;
        self.reveal = true;
        self.relayout = true;
    }

    fn select(&mut self, i: usize) {
        self.header = None;
        self.selected = i.min(self.nb.order().len().saturating_sub(1));
        self.reveal = true;
        self.relayout = true;
    }

    /// Scrolls the view by `rows`, keeping the selection on screen.
    fn scroll_by(&mut self, rows: isize) {
        let Some(g) = self.geometry else { return };
        let layout = self.layout(&g);
        let area = g.area_height();
        self.scroll = self.scroll.saturating_add_signed(rows).min(layout.max_scroll(area));
        let visible = |b: &Block| b.top + b.box_height() > self.scroll && b.top < self.scroll + area;
        if !layout.blocks.get(self.selected).is_some_and(visible) {
            let pick =
                if rows > 0 { layout.blocks.iter().position(visible) } else { layout.blocks.iter().rposition(visible) };
            if let Some(i) = pick {
                self.selected = i;
            }
        }
        self.relayout = true;
    }

    /// Gives a cell's window focus. The layout goes first, so the window exists and accepts
    /// focus when the request arrives.
    fn enter(&mut self, key: CellKey, insert: bool) {
        self.focus_seq += 1;
        self.header = None;
        self.selected = self.nb.index_of(&key).unwrap_or(self.selected);
        self.mode = Mode::Edit(key.clone());
        self.relayout();
        self.editor.enter(self.focus_seq, &key, insert);
    }

    fn leave(&mut self) {
        self.focus_seq += 1;
        self.mode = Mode::Nav;
        self.editor.leave(self.focus_seq);
        self.relayout = true;
    }

    async fn on_action(&mut self, action: Action) {
        if let Some(item) = self.header {
            match action {
                Action::Left | Action::Right => {
                    self.header = Some(if action == Action::Left { HeaderItem::Name } else { HeaderItem::Kernel });
                    self.draw = true;
                }
                Action::Edit => match item {
                    HeaderItem::Name => {
                        self.editor.calls.lua("require('nbv').rename(...)", vec![self.file_name().into()])
                    }
                    HeaderItem::Kernel => self.pick_kernel().await,
                },
                // The header sits above the first cell.
                Action::Down(n) => self.select(n - 1),
                // Selecting a cell leaves the header; the rest need no cell.
                Action::First | Action::Last(_) | Action::Help | Action::Cmdline => {
                    return self.on_cell_action(action).await;
                }
                _ => {}
            }
            return;
        }
        self.on_cell_action(action).await
    }

    async fn on_cell_action(&mut self, action: Action) {
        let len = self.nb.order().len();
        let area = self.geometry.map_or(20, |g| g.area_height()) as isize;
        let key = self.selected_key();
        match action {
            Action::Down(n) => self.select(self.selected.saturating_add(n)),
            // Past the first cell, the header.
            Action::Up(_) if self.selected == 0 => {
                self.header = Some(HeaderItem::Name);
                self.draw = true;
            }
            Action::Up(n) => self.select(self.selected.saturating_sub(n)),
            Action::First => self.select(0),
            Action::Last(None) => self.select(len.saturating_sub(1)),
            Action::Last(Some(n)) => self.select(n.saturating_sub(1)),
            Action::HalfPageDown => self.scroll_by(area / 2),
            Action::HalfPageUp => self.scroll_by(-area / 2),
            Action::Edit => match key {
                Some(k) => self.enter(k, false),
                None => {
                    let key = self.add(0);
                    self.enter(key, true);
                }
            },
            Action::Open { above } => {
                let at = if len == 0 || above { self.selected.min(len) } else { self.selected + 1 };
                self.add(at);
            }
            Action::Delete => {
                if let Some(k) = key {
                    self.register = self.nb.cell(&k).map(|c| c.raw.clone());
                    let at = self.nb.edit(vec![Change::Hide { key: k }]);
                    self.structure_changed(at);
                }
            }
            Action::Yank => {
                if let Some(k) = key {
                    self.register = self.nb.cell(&k).map(|c| c.raw.clone());
                    self.message = Some("cell yanked".into());
                    self.draw = true;
                }
            }
            Action::Paste { above } => {
                if let Some(raw) = self.register.clone() {
                    let k = self.nb.copy_cell(&raw);
                    let index = if len == 0 || above { self.selected.min(len) } else { self.selected + 1 };
                    let at = self.nb.edit(vec![Change::Show { key: k, index }]);
                    self.structure_changed(at);
                }
            }
            Action::Undo | Action::Redo => {
                let at = if action == Action::Undo { self.nb.undo() } else { self.nb.redo() };
                match at {
                    Some(i) => self.structure_changed(Some(i)),
                    None => {
                        let edge = if action == Action::Undo { "oldest" } else { "newest" };
                        self.message = Some(format!("Already at {edge} change"));
                        self.draw = true;
                    }
                }
            }
            Action::MoveDown | Action::MoveUp => {
                let to = if action == Action::MoveDown { self.selected + 1 } else { self.selected.wrapping_sub(1) };
                if let Some(k) = key.filter(|_| to < len) {
                    let at = self.nb.edit(vec![Change::Move { key: k, index: to }]);
                    self.structure_changed(at);
                }
            }
            Action::Merge => {
                if let (Some(k), Some(next)) = (key, self.nb.order().get(self.selected + 1).cloned()) {
                    let at = self.nb.edit(vec![Change::Merge { key: k, next }]);
                    self.structure_changed(at);
                }
            }
            Action::Kind(kind) => {
                if let Some(k) = key {
                    let at = self.nb.edit(vec![Change::Kind { key: k, kind }]);
                    self.structure_changed(at);
                }
            }
            Action::Run => self.run_cells(key.into_iter().collect()).await,
            Action::RunAdvance => {
                if let Some(k) = key {
                    self.run_cells(vec![k.clone()]).await;
                    self.advance(&k);
                }
            }
            Action::Interrupt => self.interrupt().await,
            Action::Restart => self.restart().await,
            Action::Cmdline => {
                self.forward_since = Some(self.editor.grid.mode_changes);
                self.editor.calls.input(":");
            }
            Action::Help => {
                let setup: &[(&str, &[(&str, &str)])] = if self.tmux_keys_missing { &[nav::TMUX_SETUP] } else { &[] };
                let sections = setup
                    .iter()
                    .chain(nav::HELP)
                    .map(|(title, keys)| {
                        let keys = keys.iter().map(|(k, d)| Value::Array(vec![(*k).into(), (*d).into()])).collect();
                        Value::Array(vec![(*title).into(), Value::Array(keys)])
                    })
                    .collect();
                self.editor.calls.lua("require('nbv').help(...)", vec![Value::Array(sections)]);
            }
            // Only the header has items side by side.
            Action::Left | Action::Right => {}
        }
    }

    fn file_name(&self) -> String {
        self.nb.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Offers the kernels found in Neovim's picker; `kernel_chosen` brings the answer.
    async fn pick_kernel(&mut self) {
        self.kernel_choices = kernel::choices().await;
        if self.kernel_choices.is_empty() {
            self.message =
                Some("no kernels found: activate a virtualenv with ipykernel, or install a kernelspec".into());
            self.draw = true;
            return;
        }
        let items = self
            .kernel_choices
            .iter()
            .map(|c| {
                let spec = c.spec.as_ref().map(|s| format!(" [{s}]")).unwrap_or_default();
                Value::from(format!("{}{spec}", c.display_name))
            })
            .collect();
        self.editor.calls.lua("require('nbv').pick_kernel(...)", vec![Value::Array(items)]);
    }

    async fn on_command(&mut self, action: &str, args: &Value) {
        let key = self.command_key(args);
        match action {
            // <S-CR> and <C-CR> in the cell being edited.
            "run" => self.run_cells(key.into_iter().collect()).await,
            "run_advance" => {
                if let Some(k) = key {
                    self.run_cells(vec![k.clone()]).await;
                    if matches!(self.mode, Mode::Edit(_)) {
                        self.leave();
                    }
                    self.advance(&k);
                }
            }
            "run_all" => {
                let keys = self.code_cells();
                self.run_cells(keys).await;
            }
            "run_above" => {
                let stop = key.and_then(|k| self.nb.index_of(&k)).unwrap_or(0);
                let above: Vec<CellKey> = self.nb.order()[..stop].to_vec();
                let keys = self.code_cells().into_iter().filter(|k| above.contains(k)).collect();
                self.run_cells(keys).await;
            }
            "split" => {
                let line = field(args, "line").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(k) = key {
                    let kind = self.nb.cell(&k).map_or(CellKind::Code, |c| c.kind());
                    let new = self.nb.new_cell(kind, "");
                    let at = self.nb.edit(vec![Change::Split { key: k, line, new: new.clone() }]);
                    if at.is_some() {
                        self.structure_changed(at);
                        // Editing continues where the cursor was: the second half.
                        self.enter(new, false);
                    }
                }
            }
            "clear_output" => {
                let all = field(args, "all").and_then(Value::as_bool).unwrap_or(false);
                let keys = if all { self.code_cells() } else { key.into_iter().collect() };
                for k in keys {
                    self.exec.clear_outputs(&mut self.nb, &k);
                    self.views.remove(&k);
                }
                self.mark_dirty();
                self.relayout = true;
            }
            "rename" => {
                let name = field(args, "name").and_then(Value::as_str).expect("the prompt sends a name");
                let from = self.nb.path().to_path_buf();
                match self.nb.rename(name) {
                    Ok(()) => {
                        self.message = Some(match self.memory.renamed(&from, self.nb.path()) {
                            Ok(()) => format!("renamed to {name}"),
                            Err(e) => e.to_string(),
                        });
                        self.editor.renamed(&self.nb);
                        self.last_sent = None;
                        self.relayout = true;
                    }
                    Err(e) => self.message = Some(e.to_string()),
                }
                self.draw = true;
            }
            "kernel_chosen" => {
                let index = field(args, "index").and_then(Value::as_u64).expect("the picker sends an index") as usize;
                let cmd = self.kernel_choices[index - 1].clone();
                if let Some(spec) = &cmd.spec {
                    // Saved with the notebook, as Jupyter does.
                    self.nb.set_kernelspec(spec, &cmd.display_name, &cmd.language);
                    self.mark_dirty();
                }
                if let Err(e) = self.memory.remember(self.nb.path(), &cmd) {
                    self.message = Some(e.to_string());
                }
                self.editor.set_language(Some(cmd.language.clone()));
                self.kernel_cmd = Some(cmd);
                self.last_sent = None;
                self.relayout = true;
                self.restart().await;
            }
            "install_ipykernel" => {
                let generation =
                    field(args, "generation").and_then(Value::as_u64).expect("the prompt sends a generation");
                let yes = field(args, "install").and_then(Value::as_bool).expect("the prompt sends an answer");
                self.install_answered(generation, yes);
            }
            "input" => {
                let value = field(args, "value").and_then(Value::as_str).unwrap_or("").to_string();
                if let (Some(req), Some(k)) = (self.input_request.take(), self.kernel.as_mut())
                    && let Err(e) = k.reply_input(&req, value).await
                {
                    self.message = Some(e.to_string());
                }
            }
            _ => {}
        }
    }

    fn view(&mut self, key: &CellKey, width: u16) -> Option<&OutputView> {
        let font = self.picker.font_size();
        if self.views.get(key).is_none_or(|v| v.width != width) {
            let cell = self.nb.cell(key)?;
            let view = outputs::build(cell, width, font, &mut self.images);
            self.views.insert(key.clone(), view);
        }
        self.views.get(key)
    }

    /// The document laid out for `g`: every cell's editor rows and output rows. A cell has the
    /// display rows Neovim measured in its window; one Neovim has not yet shown at this width
    /// has one row per line.
    fn layout(&mut self, g: &Geometry) -> Layout {
        let width = g.inner_x().1;
        let cells: Vec<(CellKey, CellKind, usize)> = self
            .nb
            .order()
            .iter()
            .filter_map(|k| {
                let c = self.nb.cell(k)?;
                let rows = self.editor.rows(k, width).unwrap_or_else(|| c.source().split('\n').count());
                Some((k.clone(), c.kind(), rows))
            })
            .collect();
        let mut items = Vec::with_capacity(cells.len());
        for (key, kind, lines) in cells {
            let outputs = if kind == CellKind::Code { self.view(&key, width).map_or(0, |v| v.height) } else { 0 };
            items.push((key, kind, lines, outputs));
        }
        Layout::new(items)
    }

    /// Lays out the notebook and sends Neovim the editor windows' places, if they changed.
    fn relayout(&mut self) {
        self.relayout = false;
        let Some(g) = self.geometry else { return };
        let layout = self.layout(&g);
        let area = g.area_height();
        self.selected = self.selected.min(layout.blocks.len().saturating_sub(1));
        match &self.mode {
            // The edited cell's box stays in view as it grows.
            Mode::Edit(k) => {
                if let Some(i) = layout.index_of(k) {
                    self.scroll = layout.reveal(self.scroll, area, i, true);
                }
            }
            _ if self.reveal => self.scroll = layout.reveal(self.scroll, area, self.selected, false),
            _ => {}
        }
        self.reveal = false;
        self.scroll = self.scroll.min(layout.max_scroll(area));
        let active = match &self.mode {
            Mode::Edit(k) => Some(k.clone()),
            _ => None,
        };
        let rects = layout.editors(&g, self.scroll, active.as_ref());
        let placement = (active, rects);
        if self.last_sent.as_ref() == Some(&placement) {
            // The windows stay put; what nbv draws around them can change with them.
            match self.sent.back_mut() {
                Some(last) => (last.1, last.2) = (layout, self.scroll),
                None => self.shown = Some((layout, self.scroll)),
            }
        } else {
            self.layout_seq += 1;
            self.editor.layout(&self.nb, self.layout_seq, placement.0.as_ref(), &placement.1);
            self.sent.push_back((self.layout_seq, layout, self.scroll));
            self.last_sent = Some(placement);
        }
        self.draw = true;
    }

    fn error_style(&self) -> Style {
        Style::default().fg(self.color("error").unwrap_or(Color::Reset)).add_modifier(Modifier::BOLD)
    }

    fn color(&self, name: &str) -> Option<Color> {
        rgb(self.theme.get(name).and_then(|c| c.0))
    }

    /// A code cell's execution count for the gutter, and its status for the box's top border.
    fn status(&self, key: &CellKey) -> (String, Option<(String, Option<Color>)>) {
        let Some(cell) = self.nb.cell(key) else { return (String::new(), None) };
        let count = cell.execution_count().map_or(" ".to_string(), |n| n.to_string());
        let stale = cell.is_stale() && cell.execution_count().is_some();
        let edited = if stale { " · edited" } else { "" };
        let ok_color = if stale { self.color("stale") } else { self.color("ok") };
        match cell.runtime.exec {
            ExecState::Queued => ("[*]".into(), Some((format!("queued{edited}"), self.color("queued")))),
            ExecState::Running => ("[*]".into(), Some((format!("running{edited}"), self.color("running")))),
            ExecState::Ok => {
                let t = cell.runtime.duration.map(|d| format!(" {:.2}s", d.as_secs_f64())).unwrap_or_default();
                (format!("[{count}]"), Some((format!("✓{t}{edited}"), ok_color)))
            }
            ExecState::Error => (format!("[{count}]"), Some((format!("✗{edited}"), self.color("error")))),
            ExecState::Idle if stale => (format!("[{count}]"), Some(("edited".into(), self.color("stale")))),
            ExecState::Idle => (format!("[{count}]"), None),
        }
    }

    fn header(&self) -> Line<'static> {
        let name = self.file_name();
        let (state, icon) = match self.exec.status() {
            KernelStatus::Starting => ("starting", "◌"),
            KernelStatus::Idle => ("idle", "○"),
            KernelStatus::Busy => ("busy", "●"),
            KernelStatus::Restarting => ("restarting", "◌"),
            KernelStatus::Dead => ("dead", "◌"),
        };
        let mode = match self.mode {
            Mode::Nav => "NAV",
            Mode::Edit(_) => "EDIT",
            Mode::Other => "",
        };
        let title = Style::default().fg(self.color("header").unwrap_or(Color::Reset)).add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(self.color("dim").unwrap_or(Color::Reset));
        let kernel = match (&self.kernel_cmd, &self.kernel_error) {
            (None, _) => Span::styled(" no kernel ", self.error_style()),
            (Some(c), Some(_)) => Span::styled(format!(" {} ✗ failed ", c.display_name), self.error_style()),
            (Some(c), None) => Span::styled(format!(" {} {icon} {state} ", c.display_name), dim),
        };
        let mark = |span: Span<'static>, item: HeaderItem| {
            if self.header != Some(item) {
                return span;
            }
            let fg = self.color("nav").unwrap_or(Color::Reset);
            span.style(Style::default().fg(fg).add_modifier(Modifier::BOLD | Modifier::REVERSED))
        };
        let mut spans = vec![
            mark(Span::styled(format!(" {name} "), title), HeaderItem::Name),
            Span::styled(format!(" {mode} {} ", self.nav.partial()), title),
            mark(kernel, HeaderItem::Kernel),
        ];
        if let Some(m) = &self.message {
            spans.push(Span::styled(format!(" {m}"), dim));
        }
        Line::from(spans)
    }

    /// How to go on from here, at the header's right end: the way to the key list, after a
    /// warning when tmux keeps `<S-CR>` and `<C-CR>` from nbv (the key list says what to add).
    fn hint(&self) -> Option<Line<'static>> {
        let text = match (&self.mode, self.header) {
            (Mode::Nav, None) => " ? help ",
            (Mode::Nav, Some(_)) => " h l select · <CR> open · ? help ",
            (Mode::Edit(_), _) => " <Esc><Esc> to cells ",
            (Mode::Other, _) => return None,
        };
        let dim = Style::default().fg(self.color("dim").unwrap_or(Color::Reset));
        let mut spans = vec![];
        if self.tmux_keys_missing {
            let warn = Style::default().fg(self.color("running").unwrap_or(Color::Reset)).add_modifier(Modifier::BOLD);
            spans.push(Span::styled(" ⚠ tmux: Shift/Ctrl+Enter off", warn));
        }
        spans.push(Span::styled(text, dim));
        Some(Line::from(spans))
    }

    fn draw(&mut self) -> anyhow::Result<()> {
        self.draw = false;
        if self.editor.grid.width == 0 {
            return Ok(());
        }
        // Views and image protocols for the outputs about to be shown.
        let visible: Vec<CellKey> = match (self.geometry, &self.shown) {
            (Some(g), Some((layout, scroll))) => layout
                .blocks
                .iter()
                .filter(|b| b.outputs > 0 && b.top + b.height() > *scroll && b.top < scroll + g.area_height())
                .map(|b| b.key.clone())
                .collect(),
            _ => vec![],
        };
        let width = self.geometry.map_or(0, |g| g.inner_x().1);
        for key in &visible {
            let Some(view) = self.view(key, width).cloned() else { continue };
            for block in &view.blocks {
                if let outputs::Block::Image { hash, image, cols, rows } = block {
                    let id = (*hash, *cols, *rows);
                    if !self.protocols.contains_key(&id)
                        && let Ok(p) =
                            SlicedProtocol::new(&self.picker, (**image).clone(), Some(Size::new(*cols, *rows)))
                    {
                        self.protocols.insert(id, p);
                    }
                }
            }
        }

        let grid = &self.editor.grid;
        let base = compose::style_of(None, grid.default_fg, grid.default_bg);
        let header = self.header();
        let hint = self.hint();
        let decorations: Vec<Decoration> = match &self.shown {
            Some((layout, _)) => layout.blocks.iter().map(|b| self.decoration(b)).collect(),
            None => vec![],
        };
        let empty_hint = self.nb.order().is_empty().then(|| {
            Line::styled(
                "empty notebook: o adds a cell",
                Style::default().fg(self.color("dim").unwrap_or(Color::Reset)),
            )
        });
        let show_cursor = !grid.busy && (self.mode != Mode::Nav || grid.mode_name != "normal");
        let cursor = show_cursor.then_some(grid.cursor);
        let (geometry, shown, views, protocols) = (self.geometry, &self.shown, &self.views, &self.protocols);
        self.terminal.draw(|f| {
            let area = f.area();
            let buf = f.buffer_mut();
            buf.set_style(area, base);
            if let Some(g) = geometry {
                let v = g.viewport;
                if v.row < area.height {
                    let width = v.width.min(area.width.saturating_sub(v.col));
                    draw_header(buf, v.col, v.row, width, &header, hint.as_ref());
                }
                if let Some(hint) = &empty_hint {
                    buf.set_line(g.inner_x().0, g.area_top(), hint, g.inner_x().1);
                }
                if let Some((layout, scroll)) = shown {
                    for (b, d) in layout.blocks.iter().zip(&decorations) {
                        draw_block(buf, &g, b, *scroll, d, views.get(&b.key), base, protocols);
                    }
                }
            }
            compose::draw_grid(buf, grid);
            if let Some((row, col)) = cursor {
                f.set_cursor_position((col as u16, row as u16));
            }
        })?;
        let mode = grid.modes.get(grid.mode).map(|m| (m.shape.clone(), m.percentage));
        if mode != self.cursor_shape {
            let style = match mode.as_ref().map(|m| &m.0) {
                Some(CursorShape::Vertical) => cursor::SetCursorStyle::SteadyBar,
                Some(CursorShape::Horizontal) => cursor::SetCursorStyle::SteadyUnderScore,
                _ => cursor::SetCursorStyle::SteadyBlock,
            };
            let mut out = std::io::stdout();
            execute!(out, style)?;
            out.flush()?;
            self.cursor_shape = mode;
        }
        Ok(())
    }

    fn decoration(&self, b: &Block) -> Decoration {
        let active = self.mode == Mode::Edit(b.key.clone());
        let selected = self.mode == Mode::Nav && self.header.is_none() && self.selected_key().as_ref() == Some(&b.key);
        let border = if active {
            self.color("edit")
        } else if selected {
            self.color("nav")
        } else {
            self.color("border")
        };
        let dim = self.color("dim");
        let (gutter, status, label) = match b.kind {
            CellKind::Code => {
                let (g, s) = self.status(&b.key);
                (g, s, None)
            }
            CellKind::Markdown => (String::new(), None, Some("markdown")),
            CellKind::Raw => (String::new(), None, Some("raw")),
        };
        Decoration { border, bold: active || selected, gutter, status, label, dim }
    }
}

/// How one block is drawn, beyond its geometry.
struct Decoration {
    border: Option<Color>,
    bold: bool,
    gutter: String,
    status: Option<(String, Option<Color>)>,
    label: Option<&'static str>,
    dim: Option<Color>,
}

/// Draws a cell's box, gutter label and outputs; the box's interior is the editor window's.
#[allow(clippy::too_many_arguments)]
fn draw_block(
    buf: &mut Buffer,
    g: &Geometry,
    b: &Block,
    scroll: usize,
    d: &Decoration,
    view: Option<&OutputView>,
    base: Style,
    protocols: &HashMap<(u64, u16, u16), SlicedProtocol>,
) {
    let (top, area) = (g.area_top(), g.area_height());
    let screen = |row: usize| (row >= scroll && row < scroll + area).then(|| top + (row - scroll) as u16);
    let (bx, bw) = g.box_x();
    let mut border = base;
    if let Some(c) = d.border {
        border = border.fg(c);
    }
    if d.bold {
        border = border.add_modifier(Modifier::BOLD);
    }
    let dim = d.dim.map_or(base, |c| base.fg(c));
    let rule = |left: &str, right: &str| format!("{left}{}{right}", "─".repeat(bw.saturating_sub(2) as usize));

    if let Some(y) = screen(b.top) {
        buf.set_string(bx, y, rule("╭", "╮"), border);
        if let Some(label) = d.label {
            buf.set_string(bx + 2, y, format!(" {label} "), dim);
        }
        if let Some((text, color)) = &d.status {
            let text = format!(" {text} ");
            let x = (bx + bw).saturating_sub(2 + text.chars().count() as u16).max(bx + 2);
            buf.set_string(x, y, text, color.map_or(dim, |c| base.fg(c)));
        }
    }
    for row in 0..b.rows {
        if let Some(y) = screen(b.top + 1 + row) {
            buf.set_string(bx, y, "│", border);
            buf.set_string(bx + bw - 1, y, "│", border);
            if row == 0 && !d.gutter.is_empty() {
                let x = bx.saturating_sub(d.gutter.chars().count() as u16 + 1).max(g.viewport.col);
                buf.set_string(x, y, &d.gutter, dim);
            }
        }
    }
    if let Some(y) = screen(b.top + b.rows + 1) {
        buf.set_string(bx, y, rule("╰", "╯"), border);
    }
    // Outputs: the visible rows of [first, first + outputs).
    let Some(view) = view.filter(|_| b.outputs > 0) else { return };
    let first = b.top + b.box_height();
    let (from, to) = (first.max(scroll), (first + b.outputs).min(scroll + area));
    if from < to {
        let (x, w) = g.inner_x();
        let region = Region {
            top: top + (from - scroll) as u16,
            len: (to - from) as u16,
            x0: x,
            x1: x + w,
            offset: from - first,
        };
        compose::draw_output(buf, &region, view, base, protocols);
    }
}

/// Draws the header row: the notebook name, mode and kernel state at the left, and the hint at the right.
/// When the screen is narrow, the header (including the kernel) remains visible, and the hint gives way.
fn draw_header(buf: &mut Buffer, col: u16, row: u16, width: u16, header: &Line, hint: Option<&Line>) {
    let header_width = header.width() as u16;
    buf.set_line(col, row, header, width);
    if let Some(hint) = hint {
        let hint_width = hint.width() as u16;
        let remaining = width.saturating_sub(header_width);
        if remaining > 0 {
            if hint_width <= remaining {
                buf.set_line(col + width - hint_width, row, hint, hint_width);
            } else {
                buf.set_line(col + header_width, row, hint, remaining);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    use super::draw_header;

    fn buffer_row_to_string(buf: &Buffer, row: u16, width: u16) -> String {
        (0..width).map(|x| buf[(x, row)].symbol()).collect()
    }

    #[test]
    fn header_keeps_kernel_visible_when_screen_is_narrow() {
        let header = Line::raw(" test.ipynb NAV python3 ○ idle ");
        let hint = Line::raw(" ? help ");
        let header_w = header.width() as u16; // 31

        // 1. Wide screen: hint is right-aligned.
        let mut buf = Buffer::empty(Rect::new(0, 0, 50, 1));
        draw_header(&mut buf, 0, 0, 50, &header, Some(&hint));
        let text = buffer_row_to_string(&buf, 0, 50);
        assert!(text.starts_with(" test.ipynb NAV python3 ○ idle "));
        assert!(text.ends_with(" ? help "));

        // 2. Narrow screen: header + hint > width, but header <= width.
        // Kernel remains visible; hint starts after header and gets cut off on the right.
        let mut buf = Buffer::empty(Rect::new(0, 0, 35, 1));
        draw_header(&mut buf, 0, 0, 35, &header, Some(&hint));
        let text = buffer_row_to_string(&buf, 0, 35);
        assert_eq!(text, " test.ipynb NAV python3 ○ idle  ? h");

        // 3. Exact width: header fits exactly, hint omitted.
        let mut buf = Buffer::empty(Rect::new(0, 0, header_w, 1));
        draw_header(&mut buf, 0, 0, header_w, &header, Some(&hint));
        let text = buffer_row_to_string(&buf, 0, header_w);
        assert_eq!(text, " test.ipynb NAV python3 ○ idle ");

        // 4. Very narrow screen: header truncated, no hint.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        draw_header(&mut buf, 0, 0, 20, &header, Some(&hint));
        let text = buffer_row_to_string(&buf, 0, 20);
        assert_eq!(text, " test.ipynb NAV pyth");
    }
}
