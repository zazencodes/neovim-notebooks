//! The event loop: terminal input, Neovim, and the kernel, around one notebook (R1).

use std::collections::HashMap;
use std::io::{Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyboardEnhancementFlags};
use crossterm::{cursor, event, execute, terminal};
use futures::StreamExt;
use jupyter_protocol::JupyterMessage;
use nbv_core::exec::{ExecEvent, Executor, KernelStatus};
use nbv_core::kernel::{self, Kernel, KernelCommand, KernelError, KernelMessage};
use nbv_core::structure::{self, Direction, Plan};
use nbv_core::{CellKey, CellKind, ExecState, Notebook};
use nbv_nvim::editor::{self, Editor, EditorEvent, field};
use nbv_nvim::redraw::CursorShape;
use nbv_nvim::{EmbeddedNvim, NvimClient, NvimError, NvimEvent};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_image::picker::Picker;
use ratatui_image::sliced::SlicedProtocol;
use rmpv::Value;
use tokio::sync::mpsc;

use crate::compose;
use crate::outputs::{self, Block, Images, OutputView};
use crate::terminal::{self as term, Tmux};

/// Placeholder highlight groups (§11.3).
const SLOTS: usize = 64;

pub struct Options {
    pub notebook: PathBuf,
    pub clean: bool,
    pub nvim: PathBuf,
}

enum KernelStart {
    Ready(u64, Result<(Kernel, KernelCommand), KernelError>),
}

struct App {
    nb: Notebook,
    editor: Editor,
    client: EmbeddedNvim,
    exec: Executor,
    kernel: Option<Kernel>,
    kernel_name: String,
    generation: u64,
    kernel_tx: mpsc::UnboundedSender<KernelMessage>,
    start_tx: mpsc::UnboundedSender<KernelStart>,
    /// Requests made before the kernel was ready.
    pending: Vec<JupyterMessage>,
    input_request: Option<JupyterMessage>,
    message: Option<String>,
    views: HashMap<CellKey, OutputView>,
    images: Images,
    protocols: HashMap<(u64, u16, u16), SlicedProtocol>,
    picker: Picker,
    tmux: Option<Tmux>,
    slots: Vec<Option<CellKey>>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
    decorations: bool,
    draw: bool,
    cursor_shape: Option<(CursorShape, u8)>,
    quit: bool,
}

/// Restores the terminal on drop, including on panic.
struct TerminalGuard {
    enhanced: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        if self.enhanced {
            let _ = execute!(out, event::PopKeyboardEnhancementFlags);
        }
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
    // Modified keys such as <S-CR> need the Kitty keyboard protocol; without it the
    // <localleader> bindings still work (§10.2).
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        execute!(out, event::PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    }
    let _guard = TerminalGuard { enhanced };
    // The capability query reads stdin, so it must run before the event stream starts.
    let picker = term::picker(tmux.as_ref());
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    let size = terminal.size()?;

    let buffer_path = editor::buffer_path(&notebook, &nb);
    let (client, mut nvim_rx) = editor::spawn(opts.nvim.clone(), opts.clean, &[], &buffer_path).await?;
    let (err_tx, mut err_rx) = mpsc::unbounded_channel::<NvimError>();
    let editor = Editor::new(client.clone(), err_tx.clone());
    {
        let (client, path) = (client.clone(), buffer_path.clone());
        let (w, h) = (size.width as usize, size.height.saturating_sub(1) as usize);
        tokio::spawn(async move {
            if let Err(e) = editor::handshake(&client, &path, w, h).await {
                let _ = err_tx.send(e);
            }
        });
    }

    let (kernel_tx, mut kernel_rx) = mpsc::unbounded_channel();
    let (start_tx, mut start_rx) = mpsc::unbounded_channel();
    let message = tmux.as_ref().and_then(|t| {
        let missing = t.missing();
        match (t.too_old(), missing.is_empty()) {
            (true, _) => Some("tmux 3.3 or newer is required for images".to_string()),
            (false, true) => None,
            (false, false) => Some(format!("tmux.conf is missing: {}", missing.join("; "))),
        }
    });
    let mut app = App {
        nb,
        editor,
        client,
        exec: Executor::default(),
        kernel: None,
        kernel_name: String::from("kernel"),
        generation: 0,
        kernel_tx,
        start_tx,
        pending: vec![],
        input_request: None,
        message,
        views: HashMap::new(),
        images: Images::default(),
        protocols: HashMap::new(),
        picker,
        tmux,
        slots: vec![None; SLOTS],
        terminal,
        decorations: false,
        draw: true,
        cursor_shape: None,
        quit: false,
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
            Some(KernelStart::Ready(generation, result)) = start_rx.recv() => app.on_kernel_ready(generation, result).await,
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
        if app.decorations {
            app.render_decorations();
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

fn arg_line(args: &Value) -> usize {
    field(args, "line").and_then(Value::as_u64).unwrap_or(0) as usize
}

fn arg_tick(args: &Value) -> u64 {
    field(args, "tick").and_then(Value::as_u64).unwrap_or(0)
}

impl App {
    fn start_kernel(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        let name = self.nb.kernel_name().map(str::to_string);
        let cwd = self.nb.path().parent().map(PathBuf::from).unwrap_or_else(|| ".".into());
        let (tx, start) = (self.kernel_tx.clone(), self.start_tx.clone());
        tokio::spawn(async move {
            let result = async {
                let cmd = kernel::resolve(name.as_deref()).await?;
                let k = Kernel::start(&cmd, &cwd, generation, tx).await?;
                Ok((k, cmd))
            }
            .await;
            let _ = start.send(KernelStart::Ready(generation, result));
        });
    }

    async fn on_kernel_ready(&mut self, generation: u64, result: Result<(Kernel, KernelCommand), KernelError>) {
        if generation != self.generation {
            if let Ok((k, _)) = result {
                tokio::spawn(k.shutdown());
            }
            return;
        }
        match result {
            Ok((mut k, cmd)) => {
                self.kernel_name = cmd.display_name;
                for msg in self.pending.drain(..) {
                    if let Err(e) = k.send_shell(msg).await {
                        self.message = Some(e.to_string());
                    }
                }
                self.kernel = Some(k);
                self.exec.reset(&mut self.nb, KernelStatus::Idle);
            }
            Err(e) => {
                self.message = Some(e.to_string());
                for ev in self.exec.reset(&mut self.nb, KernelStatus::Dead) {
                    self.on_exec_event(ev);
                }
                self.pending.clear();
            }
        }
        self.decorations = true;
        self.draw = true;
    }

    async fn on_nvim(&mut self, ev: NvimEvent) {
        for e in self.editor.handle(&mut self.nb, ev) {
            match e {
                EditorEvent::Flush => self.draw = true,
                EditorEvent::Ready | EditorEvent::DocumentChanged => self.decorations = true,
                EditorEvent::Reloaded => {
                    self.views.clear();
                    self.decorations = true;
                }
                EditorEvent::Command { action, args } => self.on_command(&action, &args).await,
                EditorEvent::Exited => self.quit = true,
            }
        }
    }

    async fn on_terminal(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => {
                if let Some(keys) = crate::keys::encode(k) {
                    self.editor.calls.request("nvim_input", vec![keys.into()]);
                }
            }
            Event::Mouse(m) => {
                if let Some((button, action, modifier, row, col)) = crate::keys::mouse(m) {
                    let args = vec![
                        button.into(),
                        action.into(),
                        modifier.into(),
                        0u64.into(),
                        (row as u64).into(),
                        (col as u64).into(),
                    ];
                    self.editor.calls.request("nvim_input_mouse", args);
                }
            }
            Event::Paste(text) => {
                self.editor.calls.request("nvim_paste", vec![text.into(), true.into(), (-1).into()]);
            }
            Event::Resize(w, h) => {
                self.editor.calls.request("nvim_ui_try_resize", vec![(w as u64).into(), (h.saturating_sub(1) as u64).into()]);
                self.views.clear();
                self.protocols.clear();
                self.decorations = true;
                self.draw = true;
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
                self.kernel = None;
                let last = stderr.lines().last().unwrap_or("").to_string();
                self.message = Some(format!("kernel died {last} (:NbvRestart to restart)"));
                for ev in self.exec.reset(&mut self.nb, KernelStatus::Dead) {
                    self.on_exec_event(ev);
                }
            }
            KernelMessage::Died { .. } => {}
        }
    }

    fn on_exec_event(&mut self, ev: ExecEvent) {
        match ev {
            ExecEvent::Cell(k) => {
                self.views.remove(&k);
                self.decorations = true;
                self.draw = true;
            }
            ExecEvent::Status(_) => self.draw = true,
            ExecEvent::InputRequested { prompt, password, .. } => {
                self.editor.calls.lua("require('nbv').input(...)", vec![prompt.into(), password.into()]);
            }
        }
    }

    fn key_at(&self, line: usize) -> Option<CellKey> {
        self.nb.span_at(line).map(|s| s.key.clone())
    }

    async fn run_cells(&mut self, keys: Vec<CellKey>) {
        for key in keys {
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
        self.decorations = true;
        self.draw = true;
    }

    fn code_cells(&self) -> Vec<CellKey> {
        self.nb.layout().iter().filter(|s| s.kind == CellKind::Code).map(|s| s.key.clone()).collect()
    }

    fn apply_plan(&mut self, tick: u64, plan: Plan) {
        self.editor.apply(tick, &plan.edits, false, plan.cursor);
    }

    async fn on_command(&mut self, action: &str, args: &Value) {
        let line = arg_line(args);
        let tick = arg_tick(args);
        match action {
            "run" => {
                let keys = self.key_at(line).into_iter().collect();
                self.run_cells(keys).await;
            }
            "run_advance" => {
                let keys = self.key_at(line).into_iter().collect();
                self.run_cells(keys).await;
                // Like Jupyter: move to the next cell, adding one at the end.
                match structure::next_cell_line(&self.nb, line) {
                    Some(next) => self.editor.apply(tick, &[], false, Some(next)),
                    None => {
                        let plan = structure::add(&mut self.nb, line, false);
                        self.apply_plan(tick, plan);
                    }
                }
            }
            "run_all" => {
                let keys = self.code_cells();
                self.run_cells(keys).await;
            }
            "run_above" => {
                let current = self.key_at(line);
                let keys = self
                    .nb
                    .layout()
                    .iter()
                    .take_while(|s| Some(&s.key) != current.as_ref())
                    .filter(|s| s.kind == CellKind::Code)
                    .map(|s| s.key.clone())
                    .collect();
                self.run_cells(keys).await;
            }
            "interrupt" => {
                if let Some(k) = self.kernel.as_mut()
                    && let Err(e) = k.interrupt().await
                {
                    self.message = Some(e.to_string());
                }
            }
            "restart" => {
                if let Some(k) = self.kernel.take() {
                    tokio::spawn(k.shutdown());
                }
                for ev in self.exec.reset(&mut self.nb, KernelStatus::Restarting) {
                    self.on_exec_event(ev);
                }
                self.pending.clear();
                self.message = None;
                self.start_kernel();
            }
            "cell_add" => {
                let above = field(args, "above").and_then(Value::as_bool).unwrap_or(false);
                let plan = structure::add(&mut self.nb, line, above);
                self.apply_plan(tick, plan);
            }
            "cell_delete" => {
                let plan = structure::delete(&self.nb, line);
                self.apply_plan(tick, plan);
            }
            "cell_split" => {
                let plan = structure::split(&mut self.nb, line);
                self.apply_plan(tick, plan);
            }
            "cell_merge" => {
                let plan = structure::merge(&self.nb, line);
                self.apply_plan(tick, plan);
            }
            "cell_move" => {
                let dir = match field(args, "dir").and_then(Value::as_str) {
                    Some("up") => Direction::Up,
                    Some("down") => Direction::Down,
                    other => {
                        self.message = Some(format!("NbvCellMove: expected up or down, got {other:?}"));
                        return;
                    }
                };
                let plan = structure::swap(&self.nb, line, dir);
                self.apply_plan(tick, plan);
            }
            "cell_type" => {
                let kind = match field(args, "kind").and_then(Value::as_str) {
                    Some("code") => CellKind::Code,
                    Some("markdown") => CellKind::Markdown,
                    Some("raw") => CellKind::Raw,
                    other => {
                        self.message = Some(format!("NbvCellType: expected code, markdown or raw, got {other:?}"));
                        return;
                    }
                };
                let plan = structure::set_type(&self.nb, line, kind);
                self.apply_plan(tick, plan);
            }
            "clear_output" => {
                let all = field(args, "all").and_then(Value::as_bool).unwrap_or(false);
                let keys = if all { self.code_cells() } else { self.key_at(line).into_iter().collect() };
                for k in keys {
                    self.exec.clear_outputs(&mut self.nb, &k);
                    self.views.remove(&k);
                }
                self.decorations = true;
                self.draw = true;
            }
            "input" => {
                let value = field(args, "value").and_then(Value::as_str).unwrap_or("").to_string();
                if let (Some(req), Some(k)) = (self.input_request.take(), self.kernel.as_mut())
                    && let Err(e) = k.reply_input(&req, value).await
                {
                    self.message = Some(e.to_string());
                }
            }
            "rerender" => self.decorations = true,
            _ => {}
        }
    }

    fn view(&mut self, key: &CellKey) -> Option<&OutputView> {
        let width = self.terminal.size().map(|s| s.width).unwrap_or(80);
        let font = self.picker.font_size();
        if self.views.get(key).is_none_or(|v| v.width != width) {
            let cell = self.nb.cell(key)?;
            let view = outputs::build(cell, width, font, &mut self.images);
            self.views.insert(key.clone(), view);
        }
        self.views.get(key)
    }

    /// Sends output placeholders and cell status to the companion (§11.1).
    fn render_decorations(&mut self) {
        self.decorations = false;
        let Some(buf) = self.editor.buffer() else { return };
        let layout = self.nb.layout().to_vec();
        let last_line = self.nb.mirror().len().saturating_sub(1);
        let mut outputs = vec![];
        let mut marks = vec![];
        let mut slots = vec![None; SLOTS];
        let mut n = 0;
        for (i, span) in layout.iter().enumerate() {
            let Some(marker) = span.marker else { continue };
            match span.kind {
                CellKind::Code => {
                    if let Some(text) = self.status_text(&span.key) {
                        marks.push(map(vec![
                            ("line", (marker as u64).into()),
                            ("text", Value::Array(vec![Value::Array(vec![text.0.into(), text.1.into()])])),
                        ]));
                    }
                    let height = self.view(&span.key).map_or(0, |v| v.height);
                    if height == 0 {
                        continue;
                    }
                    let slot = n % SLOTS;
                    n += 1;
                    slots[slot] = Some(span.key.clone());
                    // Anchor above the next cell's marker, so appended lines push output down.
                    let (line, above) = match layout.get(i + 1).and_then(|s| s.marker) {
                        Some(next) => (next, true),
                        None => (last_line, false),
                    };
                    outputs.push(map(vec![
                        ("line", (line as u64).into()),
                        ("above", above.into()),
                        ("height", (height as u64).into()),
                        ("slot", (slot as u64).into()),
                    ]));
                }
                CellKind::Markdown => {
                    marks.push(map(vec![
                        ("line", (marker as u64).into()),
                        ("text", Value::Array(vec![])),
                        ("line_hl", "NbvMarkerMarkdown".into()),
                    ]));
                }
                CellKind::Raw => {}
            }
        }
        self.slots = slots;
        self.editor.calls.lua(
            "require('nbv').render(...)",
            vec![buf.into(), self.editor.tick().into(), Value::Array(outputs), Value::Array(marks)],
        );
        self.draw = true;
    }

    /// The status shown on a code cell's marker line: execution count, state, timing.
    fn status_text(&self, key: &CellKey) -> Option<(String, &'static str)> {
        let cell = self.nb.cell(key)?;
        let count = cell.execution_count().map_or(" ".to_string(), |n| n.to_string());
        let stale = if cell.is_stale() && cell.execution_count().is_some() { " · edited" } else { "" };
        Some(match cell.runtime.exec {
            ExecState::Queued => (format!("  [*] queued{stale}"), "NbvStatusQueued"),
            ExecState::Running => (format!("  [*] running{stale}"), "NbvStatusRunning"),
            ExecState::Ok => {
                let t = cell.runtime.duration.map(|d| format!(" {:.2}s", d.as_secs_f64())).unwrap_or_default();
                (format!("  [{count}] ✓{t}{stale}"), if stale.is_empty() { "NbvStatusOk" } else { "NbvStatusStale" })
            }
            ExecState::Error => (format!("  [{count}] ✗{stale}"), "NbvStatusError"),
            ExecState::Idle if cell.execution_count().is_some() => {
                (format!("  [{count}]{stale}"), if stale.is_empty() { "NbvMarker" } else { "NbvStatusStale" })
            }
            ExecState::Idle => return None,
        })
    }

    fn slot_key(&self, slot: u16) -> Option<&CellKey> {
        self.slots.get(slot as usize).and_then(Option::as_ref)
    }

    fn draw(&mut self) -> anyhow::Result<()> {
        self.draw = false;
        let grid = &self.editor.grid;
        if grid.width == 0 {
            return Ok(());
        }
        let runs = compose::find_runs(grid, |slot| {
            self.slot_key(slot).and_then(|k| self.views.get(k)).map(|v| v.height)
        });
        // Protocols for the images about to be shown.
        for run in &runs {
            let Some(view) = self.slot_key(run.slot).and_then(|k| self.views.get(k)) else { continue };
            for block in &view.blocks {
                if let Block::Image { hash, image, cols, rows } = block {
                    let key = (*hash, *cols, *rows);
                    if !self.protocols.contains_key(&key)
                        && let Ok(p) = SlicedProtocol::new(&self.picker, (**image).clone(), Some(Size::new(*cols, *rows)))
                    {
                        self.protocols.insert(key, p);
                    }
                }
            }
        }
        let base = compose::style_of(None, grid.default_fg, grid.default_bg);
        let status = self.status_line();
        let (views, slots, protocols) = (&self.views, &self.slots, &self.protocols);
        let cursor = (!grid.busy).then_some(grid.cursor);
        self.terminal.draw(|f| {
            let area = f.area();
            let buf = f.buffer_mut();
            buf.set_style(area, base);
            for run in &runs {
                if let Some(view) = slots.get(run.slot as usize).and_then(Option::as_ref).and_then(|k| views.get(k)) {
                    compose::draw_output(buf, run, view, base, protocols);
                }
            }
            compose::draw_grid(buf, grid);
            if area.height > 0 {
                let y = area.height - 1;
                let rect = Rect { x: 0, y, width: area.width, height: 1 };
                buf.set_style(rect, base.add_modifier(Modifier::REVERSED));
                buf.set_line(0, y, &status, area.width);
            }
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

    fn status_line(&self) -> Line<'static> {
        let name = self.nb.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let state = match self.exec.status() {
            KernelStatus::Starting => "starting",
            KernelStatus::Idle => "idle",
            KernelStatus::Busy => "busy",
            KernelStatus::Restarting => "restarting",
            KernelStatus::Dead => "dead",
        };
        let icon = match self.exec.status() {
            KernelStatus::Idle => "○",
            KernelStatus::Busy => "●",
            _ => "◌",
        };
        let mut spans = vec![Span::styled(format!(" nbv  {name} "), Style::default().add_modifier(Modifier::BOLD))];
        if let Some(m) = &self.message {
            spans.push(Span::raw(format!(" {m} ")));
        }
        spans.push(Span::raw(format!(" {} {icon} {state} ", self.kernel_name)));
        Line::from(spans)
    }
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
