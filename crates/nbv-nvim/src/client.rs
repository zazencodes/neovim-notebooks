//! The `NvimClient` boundary (§5.3). `nvim-rs` types stay inside this module; a breaking
//! upstream release is a repair here and nowhere else.

use nbv_core::diagnostics;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use nvim_rs::compat::tokio::Compat;
use nvim_rs::{Handler, Neovim};
use rmpv::Value;
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::redraw::{self, RedrawEvent};

/// Everything Neovim tells nbv, in the order Neovim sent it.
#[derive(Debug)]
pub enum NvimEvent {
    Redraw(Vec<RedrawEvent>),
    /// `nvim_buf_lines_event`: lines `[first, last)` replaced; `last == -1` is the whole buffer.
    BufLines {
        buf: i64,
        tick: Option<u64>,
        first: i64,
        last: i64,
        lines: Vec<String>,
    },
    /// `nvim_buf_changedtick_event`: the tick moved without a text change (e.g. after `:w`).
    BufChangedTick {
        buf: i64,
        tick: u64,
    },
    BufDetach {
        buf: i64,
    },
    /// An `rpcnotify` from the companion.
    Notify {
        name: String,
        args: Vec<Value>,
    },
    /// An `rpcrequest` from the companion. Neovim blocks until `reply` is answered, so the
    /// answer must not depend on a request back to Neovim.
    Request {
        name: String,
        args: Vec<Value>,
        reply: oneshot::Sender<Value>,
    },
    Exited {
        error: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum NvimError {
    #[error("cannot start {0}: {1}")]
    Spawn(String, std::io::Error),
    #[error("{0}")]
    Rpc(String),
}

pub struct SpawnOptions {
    pub program: PathBuf,
    /// Launch without user configuration (`nbv --clean`, §10.5).
    pub clean: bool,
    pub args: Vec<String>,
    /// Environment variables set for Neovim on top of nbv's own.
    pub env: Vec<(String, PathBuf)>,
}

/// The minimal surface nbv needs from an embedded Neovim.
pub trait NvimClient: Clone + Send + Sync + 'static {
    fn request(&self, method: &str, args: Vec<Value>) -> impl Future<Output = Result<Value, NvimError>> + Send;

    fn exec_lua(&self, code: &str, args: Vec<Value>) -> impl Future<Output = Result<Value, NvimError>> + Send {
        self.request("nvim_exec_lua", vec![code.into(), Value::Array(args)])
    }

    fn attach_ui(&self, width: usize, height: usize) -> impl Future<Output = Result<(), NvimError>> + Send {
        let opts = Value::Map(vec![
            ("rgb".into(), true.into()),
            ("ext_linegrid".into(), true.into()),
            ("ext_hlstate".into(), true.into()),
        ]);
        async move {
            self.request("nvim_ui_attach", vec![(width as u64).into(), (height as u64).into(), opts]).await?;
            Ok(())
        }
    }

    fn resize(&self, width: usize, height: usize) -> impl Future<Output = Result<(), NvimError>> + Send {
        async move {
            self.request("nvim_ui_try_resize", vec![(width as u64).into(), (height as u64).into()]).await?;
            Ok(())
        }
    }

    fn input(&self, keys: &str) -> impl Future<Output = Result<(), NvimError>> + Send {
        let keys = keys.to_string();
        async move {
            self.request("nvim_input", vec![keys.into()]).await?;
            Ok(())
        }
    }

    /// Subscribes to buffer updates. With `send_buffer`, the first event carries every line.
    fn attach_buffer(&self, buf: i64, send_buffer: bool) -> impl Future<Output = Result<(), NvimError>> + Send {
        async move {
            self.request("nvim_buf_attach", vec![buf.into(), send_buffer.into(), Value::Map(vec![])]).await?;
            Ok(())
        }
    }

    fn buffer_lines(&self, buf: i64) -> impl Future<Output = Result<Vec<String>, NvimError>> + Send {
        async move {
            let v = self.request("nvim_buf_get_lines", vec![buf.into(), 0.into(), (-1).into(), false.into()]).await?;
            Ok(strings(&v))
        }
    }

    fn quit(&self) -> impl Future<Output = Result<(), NvimError>> + Send {
        async move {
            let _ = self.request("nvim_command", vec!["qall!".into()]).await;
            Ok(())
        }
    }
}

/// Neovim over `--embed` stdio.
#[derive(Clone)]
pub struct EmbeddedNvim {
    nvim: Neovim<Compat<ChildStdin>>,
    quitting: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Forwarder {
    tx: mpsc::UnboundedSender<NvimEvent>,
}

/// A `buffer` ext value carries the handle as msgpack.
fn buffer_handle(v: &Value) -> i64 {
    match v {
        Value::Ext(_, data) => {
            rmpv::decode::read_value(&mut data.as_slice()).ok().and_then(|v| v.as_i64()).unwrap_or(-1)
        }
        v => v.as_i64().unwrap_or(-1),
    }
}

pub fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|l| match l {
            Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
            Value::Binary(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        })
        .collect()
}

#[async_trait::async_trait]
impl Handler for Forwarder {
    type Writer = Compat<ChildStdin>;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _: Neovim<Self::Writer>) {
        let ev = match name.as_str() {
            "redraw" => NvimEvent::Redraw(redraw::parse(&args)),
            "nvim_buf_lines_event" => NvimEvent::BufLines {
                buf: buffer_handle(&args[0]),
                tick: args.get(1).and_then(Value::as_u64),
                first: args.get(2).and_then(Value::as_i64).unwrap_or(0),
                last: args.get(3).and_then(Value::as_i64).unwrap_or(0),
                lines: args.get(4).map(strings).unwrap_or_default(),
            },
            "nvim_buf_detach_event" => NvimEvent::BufDetach { buf: buffer_handle(&args[0]) },
            "nvim_buf_changedtick_event" => NvimEvent::BufChangedTick {
                buf: buffer_handle(&args[0]),
                tick: args.get(1).and_then(Value::as_u64).unwrap_or(0),
            },
            _ => NvimEvent::Notify { name, args },
        };
        let _ = self.tx.send(ev);
    }

    async fn handle_request(&self, name: String, args: Vec<Value>, _: Neovim<Self::Writer>) -> Result<Value, Value> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(NvimEvent::Request { name, args, reply });
        rx.await.map_err(|_| Value::from("nbv is shutting down"))
    }
}

impl EmbeddedNvim {
    /// Starts `nvim --embed`. It never touches the terminal (§5.1).
    pub async fn spawn(opts: SpawnOptions) -> Result<(EmbeddedNvim, mpsc::UnboundedReceiver<NvimEvent>), NvimError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut cmd = Command::new(&opts.program);
        cmd.arg("--embed");
        if opts.clean {
            cmd.arg("--clean");
        }
        cmd.args(&opts.args).envs(opts.env.iter().map(|(k, v)| (k, v))).stderr(Stdio::piped()).kill_on_drop(true);
        let (nvim, mut io, mut child) = nvim_rs::create::tokio::new_child_cmd(&mut cmd, Forwarder { tx: tx.clone() })
            .await
            .map_err(|e| NvimError::Spawn(opts.program.display().to_string(), e))?;
        let quitting = Arc::new(AtomicBool::new(false));
        let expected = quitting.clone();
        let tail = diagnostics::stderr_tail();
        let stderr = child.stderr.take().expect("piped stderr");
        let capture = tail.clone();
        let mut stderr_task = tokio::spawn(async move { diagnostics::read_stderr(stderr, capture).await });
        tokio::spawn(async move {
            let mut exit_status = None;
            let rpc_result = tokio::select! {
                result = &mut io => Some(result),
                status = child.wait() => {
                    exit_status = Some(status);
                    match tokio::time::timeout(Duration::from_millis(500), &mut io).await {
                        Ok(result) => Some(result),
                        Err(_) => { io.abort(); None }
                    }
                }
            };
            let (rpc_closed, rpc) = match rpc_result {
                Some(Ok(Ok(()))) => (true, None),
                Some(Ok(Err(e))) => (e.is_channel_closed(), Some(rpc_loop_error(&e))),
                Some(Err(e)) => (false, Some(e.to_string())),
                None => (false, Some("RPC did not close after process exit".into())),
            };
            // An RPC failure with a still-live child must not hang the application.
            let status = match exit_status {
                Some(status) => status,
                None => match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                    Ok(status) => status,
                    Err(_) => {
                        let _ = child.kill().await;
                        child.wait().await
                    }
                },
            };
            let stderr_result = tokio::time::timeout(Duration::from_millis(500), &mut stderr_task).await;
            if stderr_result.is_err() {
                stderr_task.abort();
            }
            let clean = status.as_ref().is_ok_and(|s| s.success());
            let status = diagnostics::exit_status(&status);
            let error = if (clean && rpc_closed) || expected.load(Ordering::SeqCst) {
                None
            } else {
                Some(format!(
                    "embedded Neovim exited unexpectedly ({status}); RPC: {}; stderr: {}{}. Try nvb --clean to check your configuration. Lifecycle log: {}",
                    rpc.as_deref().unwrap_or("closed"),
                    diagnostics::tail_text(&tail),
                    if stderr_result.is_err() { " [stderr did not close]" } else { "" },
                    diagnostics::log_path().map(|p| p.display().to_string()).unwrap_or_else(|e| e.to_string())
                ))
            };
            let event = if error.is_some() { "unexpected exit" } else { "quit" };
            let _ = diagnostics::record("neovim", &format!("{event} {status}"), 0);
            let _ = tx.send(NvimEvent::Exited { error });
        });
        Ok((EmbeddedNvim { nvim, quitting }, rx))
    }
}

impl NvimClient for EmbeddedNvim {
    async fn quit(&self) -> Result<(), NvimError> {
        self.quitting.store(true, Ordering::SeqCst);
        let _ = self.request("nvim_command", vec!["qall!".into()]).await;
        Ok(())
    }

    async fn request(&self, method: &str, args: Vec<Value>) -> Result<Value, NvimError> {
        match self.nvim.call(method, args).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(NvimError::Rpc(format!("{method}: {}", describe(&e)))),
            Err(e) => Err(NvimError::Rpc(format!("{method}: {e}"))),
        }
    }
}

/// Loop errors may contain complete response values. Keep the failure category and
/// request identity without copying buffer contents or user input into diagnostics.
fn rpc_loop_error(error: &nvim_rs::error::LoopError) -> String {
    use nvim_rs::error::LoopError;
    match error {
        LoopError::DecodeError(error, _) => format!("RPC decode failed: {error}"),
        LoopError::MsgidNotFound(id) => format!("RPC response has unknown request id {id}"),
        LoopError::InternalSendResponseError(id, _) => format!("cannot deliver RPC response for request {id}"),
    }
}

/// Neovim errors arrive as `[type, message]`.
fn describe(v: &Value) -> String {
    match v.as_array().and_then(|a| a.get(1)).and_then(Value::as_str) {
        Some(msg) => msg.to_string(),
        None => v.to_string(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    async fn fake_process(script: &str) -> NvimEvent {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("fake-nvim");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_client, mut rx) =
            EmbeddedNvim::spawn(SpawnOptions { program, clean: false, args: vec![], env: vec![] }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn preserves_stderr_status_and_rpc_failure() {
        let event = fake_process("echo process-fault >&2; printf '\\301'; exit 23").await;
        let NvimEvent::Exited { error: Some(error) } = event else { panic!("{event:?}") };
        assert!(error.contains("process-fault"), "{error}");
        assert!(error.contains("RPC:"), "{error}");
        assert!(error.contains("23"), "{error}");
        assert!(error.contains("--clean"), "{error}");
    }

    #[tokio::test]
    async fn rpc_failure_kills_and_reaps_a_child_that_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let event = fake_process(&format!("echo $$ > '{}'; printf '\\301'; exec sleep 60", pid_file.display())).await;
        assert!(matches!(event, NvimEvent::Exited { error: Some(_) }));
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap();
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "RPC failure must reap its child");
    }

    #[tokio::test]
    async fn malformed_rpc_is_reported_even_with_zero_exit_status() {
        assert!(matches!(fake_process("printf '\\301'; exit 0").await, NvimEvent::Exited { error: Some(_) }));
    }

    #[tokio::test]
    async fn normal_quit_is_expected() {
        assert!(matches!(fake_process("exit 0").await, NvimEvent::Exited { error: None }));
    }
}
