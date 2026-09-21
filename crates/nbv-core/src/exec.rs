//! The Jupyter-level execution state machine (§13). Transport-free: it consumes messages
//! and applies them to the notebook against the owning `CellKey`.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use jupyter_protocol::{
    ExecuteRequest, ExecutionState, JupyterMessage, JupyterMessageContent, ReplyStatus, Stdio,
};
use serde_json::{Map, Value, json};

use crate::document::ExecState;
use crate::key::CellKey;
use crate::notebook::Notebook;

/// What the kernel is doing, from the frontend's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KernelStatus {
    #[default]
    Starting,
    Idle,
    Busy,
    Restarting,
    Dead,
}

/// Something the frontend should react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecEvent {
    /// A cell's outputs, execution count or state changed.
    Cell(CellKey),
    Status(KernelStatus),
    /// The kernel asked for input on behalf of a cell (`input()`).
    InputRequested { key: Option<CellKey>, prompt: String, password: bool },
}

#[derive(Default, Debug)]
pub struct Executor {
    /// execute_request `msg_id` → cell.
    requests: HashMap<String, CellKey>,
    started: HashMap<CellKey, Instant>,
    /// Cells whose outputs clear on the next output (`clear_output(wait=True)`).
    clear_pending: HashSet<CellKey>,
    /// display_id → outputs carrying it.
    displays: HashMap<String, Vec<(CellKey, usize)>>,
    status: KernelStatus,
}

impl Executor {
    pub fn status(&self) -> KernelStatus {
        self.status
    }

    /// Builds the `execute_request` for `key` and records it. `None` if the cell is not a live
    /// code cell. The request's source becomes the cell's staleness baseline (§7.6).
    pub fn request(&mut self, nb: &mut Notebook, key: &CellKey) -> Option<JupyterMessage> {
        if !nb.is_live(key) || nb.cell(key)?.kind() != crate::CellKind::Code {
            return None;
        }
        let cell = nb.cell_mut(key)?;
        let code = cell.source();
        cell.runtime.baseline = code.clone();
        cell.runtime.exec = ExecState::Queued;
        let request = ExecuteRequest {
            code,
            silent: false,
            store_history: true,
            user_expressions: None,
            allow_stdin: true,
            stop_on_error: true,
        };
        let msg = JupyterMessage::new(request, None);
        self.requests.insert(msg.header.msg_id.clone(), key.clone());
        Some(msg)
    }

    /// Cells queued or running. They become idle when the kernel restarts or dies.
    pub fn in_flight(&self) -> impl Iterator<Item = &CellKey> {
        self.requests.values()
    }

    /// Clears a cell's outputs at the user's request.
    pub fn clear_outputs(&mut self, nb: &mut Notebook, key: &CellKey) {
        if let Some(cell) = nb.cell_mut(key) {
            if cell.outputs().is_empty() && cell.execution_count().is_none() {
                return;
            }
            cell.outputs_mut().clear();
            if cell.raw.contains_key("execution_count") {
                cell.raw.insert("execution_count".into(), Value::Null);
            }
            cell.runtime.exec = ExecState::Idle;
            cell.runtime.duration = None;
        }
        self.forget_displays(key);
    }

    /// The kernel went away (restart or death): nothing in flight will complete.
    pub fn reset(&mut self, nb: &mut Notebook, status: KernelStatus) -> Vec<ExecEvent> {
        let mut events = vec![];
        for (_, key) in self.requests.drain() {
            if let Some(rt) = nb.runtime_mut(&key) {
                rt.exec = ExecState::Idle;
            }
            events.push(ExecEvent::Cell(key));
        }
        self.started.clear();
        self.clear_pending.clear();
        self.status = status;
        events.push(ExecEvent::Status(status));
        events
    }

    /// Applies one message from the iopub, shell or stdin channel.
    pub fn handle(&mut self, nb: &mut Notebook, msg: &JupyterMessage) -> Vec<ExecEvent> {
        let parent = msg.parent_header.as_ref().map(|h| h.msg_id.as_str());
        let key = parent.and_then(|p| self.requests.get(p)).cloned();
        let mut events = vec![];

        match &msg.content {
            JupyterMessageContent::Status(s) => {
                let status = match s.execution_state {
                    ExecutionState::Busy => KernelStatus::Busy,
                    ExecutionState::Idle => KernelStatus::Idle,
                    ExecutionState::Starting => KernelStatus::Starting,
                    ExecutionState::Restarting | ExecutionState::AutoRestarting => KernelStatus::Restarting,
                    ExecutionState::Dead | ExecutionState::Terminating => KernelStatus::Dead,
                    _ => self.status,
                };
                if status != self.status {
                    self.status = status;
                    events.push(ExecEvent::Status(status));
                }
                if let (Some(k), KernelStatus::Busy) = (&key, status)
                    && let Some(rt) = nb.runtime_mut(k)
                    && rt.exec == ExecState::Queued
                {
                    rt.exec = ExecState::Running;
                    self.started.insert(k.clone(), Instant::now());
                    // Jupyter clears previous outputs when execution starts.
                    self.clear_outputs_for_run(nb, k);
                    events.push(ExecEvent::Cell(k.clone()));
                }
            }
            JupyterMessageContent::ExecuteInput(input) => {
                if let Some(k) = &key
                    && let Some(cell) = nb.cell_mut(k)
                {
                    cell.raw.insert("execution_count".into(), json!(input.execution_count.value()));
                    events.push(ExecEvent::Cell(k.clone()));
                }
            }
            JupyterMessageContent::StreamContent(s) => {
                if let Some(k) = &key {
                    let name = match s.name {
                        Stdio::Stdout => "stdout",
                        Stdio::Stderr => "stderr",
                    };
                    self.push_stream(nb, k, name, &s.text);
                    events.push(ExecEvent::Cell(k.clone()));
                }
            }
            JupyterMessageContent::DisplayData(d) => {
                if let Some(k) = &key {
                    let output = json!({
                        "data": media_json(&d.data),
                        "metadata": Value::Object(d.metadata.clone()),
                        "output_type": "display_data",
                    });
                    let id = d.transient.as_ref().and_then(|t| t.display_id.clone());
                    self.push_output(nb, k, output, id);
                    events.push(ExecEvent::Cell(k.clone()));
                }
            }
            JupyterMessageContent::ExecuteResult(r) => {
                if let Some(k) = &key {
                    let output = json!({
                        "data": media_json(&r.data),
                        "execution_count": r.execution_count.value(),
                        "metadata": Value::Object(r.metadata.clone()),
                        "output_type": "execute_result",
                    });
                    let id = r.transient.as_ref().and_then(|t| t.display_id.clone());
                    self.push_output(nb, k, output, id);
                    events.push(ExecEvent::Cell(k.clone()));
                }
            }
            JupyterMessageContent::ErrorOutput(e) => {
                if let Some(k) = &key {
                    let output = json!({
                        "ename": e.ename,
                        "evalue": e.evalue,
                        "output_type": "error",
                        "traceback": e.traceback,
                    });
                    self.push_output(nb, k, output, None);
                    events.push(ExecEvent::Cell(k.clone()));
                }
            }
            JupyterMessageContent::ClearOutput(c) => {
                if let Some(k) = &key {
                    if c.wait {
                        self.clear_pending.insert(k.clone());
                    } else {
                        self.clear_now(nb, k);
                        events.push(ExecEvent::Cell(k.clone()));
                    }
                }
            }
            JupyterMessageContent::UpdateDisplayData(u) => {
                // Updates apply to every output carrying the id, whichever cell produced it.
                let Some(id) = &u.transient.display_id else { return events };
                let targets = self.displays.get(id).cloned().unwrap_or_default();
                for (k, idx) in targets {
                    if let Some(cell) = nb.cell_mut(&k)
                        && let Some(Value::Object(out)) = cell.outputs_mut().get_mut(idx)
                    {
                        out.insert("data".into(), media_json(&u.data));
                        out.insert("metadata".into(), Value::Object(u.metadata.clone()));
                        events.push(ExecEvent::Cell(k));
                    }
                }
            }
            JupyterMessageContent::ExecuteReply(r) => {
                if let Some(p) = parent
                    && let Some(k) = self.requests.remove(p)
                {
                    let started = self.started.remove(&k);
                    self.clear_pending.remove(&k);
                    if let Some(cell) = nb.cell_mut(&k) {
                        cell.runtime.exec = match r.status {
                            ReplyStatus::Ok => ExecState::Ok,
                            ReplyStatus::Error => ExecState::Error,
                            ReplyStatus::Aborted => ExecState::Idle,
                        };
                        if r.status != ReplyStatus::Aborted {
                            cell.raw.insert("execution_count".into(), json!(r.execution_count.value()));
                        }
                        cell.runtime.duration = started.map(|s| s.elapsed());
                    }
                    events.push(ExecEvent::Cell(k));
                }
            }
            JupyterMessageContent::InputRequest(r) => {
                events.push(ExecEvent::InputRequested { key, prompt: r.prompt.clone(), password: r.password });
            }
            _ => {}
        }
        events
    }

    fn clear_outputs_for_run(&mut self, nb: &mut Notebook, key: &CellKey) {
        if let Some(cell) = nb.cell_mut(key) {
            cell.outputs_mut().clear();
            cell.raw.insert("execution_count".into(), Value::Null);
        }
        self.forget_displays(key);
    }

    fn clear_now(&mut self, nb: &mut Notebook, key: &CellKey) {
        self.clear_pending.remove(key);
        if let Some(cell) = nb.cell_mut(key) {
            cell.outputs_mut().clear();
        }
        self.forget_displays(key);
    }

    fn forget_displays(&mut self, key: &CellKey) {
        for targets in self.displays.values_mut() {
            targets.retain(|(k, _)| k != key);
        }
        self.displays.retain(|_, t| !t.is_empty());
    }

    fn push_output(&mut self, nb: &mut Notebook, key: &CellKey, output: Value, display_id: Option<String>) {
        if self.clear_pending.contains(key) {
            self.clear_now(nb, key);
        }
        let Some(cell) = nb.cell_mut(key) else { return };
        let outputs = cell.outputs_mut();
        outputs.push(output);
        if let Some(id) = display_id {
            self.displays.entry(id).or_default().push((key.clone(), outputs.len() - 1));
        }
    }

    /// Consecutive stream output of the same name merges into one output, as Jupyter does.
    fn push_stream(&mut self, nb: &mut Notebook, key: &CellKey, name: &str, text: &str) {
        if self.clear_pending.contains(key) {
            self.clear_now(nb, key);
        }
        let Some(cell) = nb.cell_mut(key) else { return };
        let outputs = cell.outputs_mut();
        if let Some(Value::Object(last)) = outputs.last_mut()
            && last.get("output_type").and_then(Value::as_str) == Some("stream")
            && last.get("name").and_then(Value::as_str) == Some(name)
        {
            let merged = collapse_cr(&(crate::document::multiline(last.get("text")) + text));
            last.insert("text".into(), lines_json(&merged));
            return;
        }
        outputs.push(json!({ "name": name, "output_type": "stream", "text": lines_json(&collapse_cr(text)) }));
    }
}

/// Applies carriage returns within each line, so progress bars do not accumulate.
fn collapse_cr(s: &str) -> String {
    s.split_inclusive('\n')
        .map(|line| {
            let (body, nl) = line.strip_suffix('\n').map_or((line, ""), |b| (b, "\n"));
            // A trailing \r keeps its text visible until something overwrites it.
            let trimmed = body.trim_end_matches('\r');
            let kept = trimmed.rsplit('\r').next().unwrap_or("");
            let tail = &body[trimmed.len()..];
            format!("{kept}{tail}{nl}")
        })
        .collect()
}

/// nbformat's multiline list form.
fn lines_json(s: &str) -> Value {
    Value::Array(crate::document::split_keep_newlines(s).into_iter().map(Value::String).collect())
}

/// A MIME bundle in nbformat form: text types split into lines, JSON types as objects.
fn media_json(media: &jupyter_protocol::Media) -> Value {
    let Ok(Value::Object(bundle)) = serde_json::to_value(media) else { return json!({}) };
    let out: Map<String, Value> = bundle
        .into_iter()
        .map(|(mime, v)| {
            let split = mime.starts_with("text/") || mime == "application/javascript" || mime == "image/svg+xml";
            let v = match v {
                Value::String(s) if split => lines_json(&s),
                v => v,
            };
            (mime, v)
        })
        .collect();
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jupyter_protocol::{
        ClearOutput, DisplayData, ErrorOutput, ExecuteInput, ExecuteReply, ExecutionCount, Media,
        MediaType, Status, StreamContent, Transient, UpdateDisplayData,
    };
    use std::path::Path;

    fn notebook() -> Notebook {
        let json = br#"{"cells":[{"cell_type":"code","execution_count":null,"id":"a","metadata":{},"outputs":[],"source":["print(1)"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;
        Notebook::from_bytes(Path::new("t.ipynb"), json, Default::default()).unwrap()
    }

    fn setup() -> (Notebook, Executor, JupyterMessage, CellKey) {
        let mut nb = notebook();
        let mut ex = Executor::default();
        let key = CellKey::new("a");
        let req = ex.request(&mut nb, &key).unwrap();
        match &req.content {
            JupyterMessageContent::ExecuteRequest(r) => assert_eq!(r.code, "print(1)"),
            other => panic!("{other:?}"),
        }
        (nb, ex, req, key)
    }

    fn reply(parent: &JupyterMessage, status: ReplyStatus, n: usize) -> JupyterMessage {
        let r = ExecuteReply {
            status,
            execution_count: ExecutionCount::new(n),
            payload: vec![],
            user_expressions: None,
            error: None,
        };
        JupyterMessage::new(r, Some(parent))
    }

    #[test]
    fn a_full_execution() {
        let (mut nb, mut ex, req, key) = setup();
        ex.handle(&mut nb, &JupyterMessage::new(Status::busy(), Some(&req)));
        assert_eq!(nb.cell(&key).unwrap().runtime.exec, ExecState::Running);
        ex.handle(&mut nb, &JupyterMessage::new(ExecuteInput { code: "".into(), execution_count: ExecutionCount::new(7) }, Some(&req)));
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stdout("1\n"), Some(&req)));
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stdout("2\n"), Some(&req)));
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stderr("warn\n"), Some(&req)));
        ex.handle(&mut nb, &reply(&req, ReplyStatus::Ok, 7));
        let cell = nb.cell(&key).unwrap();
        assert_eq!(cell.runtime.exec, ExecState::Ok);
        assert_eq!(cell.execution_count(), Some(7));
        assert!(cell.runtime.duration.is_some());
        assert_eq!(
            Value::Array(cell.outputs().to_vec()),
            json!([
                {"name": "stdout", "output_type": "stream", "text": ["1\n", "2\n"]},
                {"name": "stderr", "output_type": "stream", "text": ["warn\n"]},
            ])
        );
        assert!(!cell.is_stale());
        assert_eq!(ex.in_flight().count(), 0);
    }

    #[test]
    fn errors_and_carriage_returns() {
        let (mut nb, mut ex, req, key) = setup();
        ex.handle(&mut nb, &JupyterMessage::new(Status::busy(), Some(&req)));
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stdout("10%\r"), Some(&req)));
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stdout("50%\r100%\ndone\n"), Some(&req)));
        let err = ErrorOutput { ename: "E".into(), evalue: "v".into(), traceback: vec!["tb".into()] };
        ex.handle(&mut nb, &JupyterMessage::new(err, Some(&req)));
        ex.handle(&mut nb, &reply(&req, ReplyStatus::Error, 3));
        let cell = nb.cell(&key).unwrap();
        assert_eq!(cell.runtime.exec, ExecState::Error);
        assert_eq!(cell.outputs()[0]["text"], json!(["100%\n", "done\n"]));
        assert_eq!(cell.outputs()[1]["output_type"], "error");
    }

    #[test]
    fn clear_output_wait_and_display_updates() {
        let (mut nb, mut ex, req, key) = setup();
        ex.handle(&mut nb, &JupyterMessage::new(Status::busy(), Some(&req)));
        let media = |s: &str| Media::new(vec![MediaType::Plain(s.into())]);
        let mut d = DisplayData::new(media("one"));
        d.transient = Some(Transient { display_id: Some("disp".into()) });
        ex.handle(&mut nb, &JupyterMessage::new(d, Some(&req)));
        ex.handle(&mut nb, &JupyterMessage::new(UpdateDisplayData::new(media("two"), "disp"), Some(&req)));
        assert_eq!(nb.cell(&key).unwrap().outputs()[0]["data"]["text/plain"], json!(["two"]));

        ex.handle(&mut nb, &JupyterMessage::new(ClearOutput { wait: true }, Some(&req)));
        assert_eq!(nb.cell(&key).unwrap().outputs().len(), 1, "wait defers the clear");
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stdout("frame\n"), Some(&req)));
        let outputs = nb.cell(&key).unwrap().outputs().to_vec();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["text"], json!(["frame\n"]));
    }

    #[test]
    fn output_for_a_tombstoned_cell_lands_on_the_tombstone() {
        let (mut nb, mut ex, req, key) = setup();
        ex.handle(&mut nb, &JupyterMessage::new(Status::busy(), Some(&req)));
        nb.resync(vec![]);
        assert!(nb.is_tombstoned(&key));
        ex.handle(&mut nb, &JupyterMessage::new(StreamContent::stdout("late\n"), Some(&req)));
        ex.handle(&mut nb, &reply(&req, ReplyStatus::Ok, 1));
        assert_eq!(nb.cell(&key).unwrap().outputs().len(), 1);
    }

    #[test]
    fn aborted_and_reset_cells_return_to_idle() {
        let (mut nb, mut ex, req, key) = setup();
        ex.handle(&mut nb, &reply(&req, ReplyStatus::Aborted, 0));
        assert_eq!(nb.cell(&key).unwrap().runtime.exec, ExecState::Idle);
        assert_eq!(nb.cell(&key).unwrap().execution_count(), None);

        ex.request(&mut nb, &key).unwrap();
        ex.reset(&mut nb, KernelStatus::Restarting);
        assert_eq!(nb.cell(&key).unwrap().runtime.exec, ExecState::Idle);
        assert_eq!(ex.in_flight().count(), 0);
    }

    #[test]
    fn collapse_cr_keeps_pending_text() {
        assert_eq!(collapse_cr("a\rb\rc\n"), "c\n");
        assert_eq!(collapse_cr("abc\r"), "abc\r");
        assert_eq!(collapse_cr("x\ny\rz"), "x\nz");
    }
}
