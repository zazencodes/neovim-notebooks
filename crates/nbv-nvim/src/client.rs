//! The `NvimClient` boundary (§5.3). `nvim-rs` types stay inside this module; a breaking
//! upstream release is a repair here and nowhere else.

use std::path::PathBuf;
use std::process::Stdio;

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
    Exited,
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
        cmd.args(&opts.args).envs(opts.env.iter().map(|(k, v)| (k, v))).stderr(Stdio::null()).kill_on_drop(true);
        let (nvim, io, mut child) = nvim_rs::create::tokio::new_child_cmd(&mut cmd, Forwarder { tx: tx.clone() })
            .await
            .map_err(|e| NvimError::Spawn(opts.program.display().to_string(), e))?;
        tokio::spawn(async move {
            let _ = io.await;
            let _ = child.wait().await;
            let _ = tx.send(NvimEvent::Exited);
        });
        Ok((EmbeddedNvim { nvim }, rx))
    }
}

impl NvimClient for EmbeddedNvim {
    async fn request(&self, method: &str, args: Vec<Value>) -> Result<Value, NvimError> {
        match self.nvim.call(method, args).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(NvimError::Rpc(format!("{method}: {}", describe(&e)))),
            Err(e) => Err(NvimError::Rpc(format!("{method}: {e}"))),
        }
    }
}

/// Neovim errors arrive as `[type, message]`.
fn describe(v: &Value) -> String {
    match v.as_array().and_then(|a| a.get(1)).and_then(Value::as_str) {
        Some(msg) => msg.to_string(),
        None => v.to_string(),
    }
}
