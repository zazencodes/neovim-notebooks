//! Kernel transport and lifecycle over `jupyter-zmq-client` (§13). Wire details belong to
//! the library; this module launches, connects, forwards messages, interrupts and stops.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use jupyter_protocol::{
    ConnectionInfo, InputReply, InterruptRequest, JupyterMessage, JupyterMessageContent, KernelInfoRequest,
    ReplyStatus, ShutdownRequest, Transport,
};
use jupyter_zmq_client as zmq;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// How to start a kernel.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KernelCommand {
    /// `{connection_file}` is replaced with the connection file path.
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub interrupt_via_message: bool,
    pub display_name: String,
    /// The language of code cells, lowercased as editors name it (`python`, `r`).
    pub language: String,
    /// The installed kernelspec this comes from, if any.
    pub spec: Option<String>,
}

/// What a running kernel reports to the application loop.
#[derive(Debug)]
pub enum KernelMessage {
    Message(Box<JupyterMessage>),
    /// The kernel process exited. Carries the tail of its stderr.
    Died {
        generation: u64,
        stderr: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("no kernel for {0:?}: {1}")]
    NotFound(String, String),
    #[error("cannot start kernel: {0}")]
    Spawn(std::io::Error),
    #[error("kernel did not answer within {0:?}{1}")]
    Timeout(Duration, String),
    #[error("kernel connection: {0}")]
    Connection(String),
    #[error("cannot install ipykernel: {0}")]
    Install(String),
}

fn conn_err(e: impl std::fmt::Display) -> KernelError {
    KernelError::Connection(e.to_string())
}

fn launcher(python: String, display_name: String) -> KernelCommand {
    KernelCommand {
        argv: vec![python, "-m".into(), "ipykernel_launcher".into(), "-f".into(), "{connection_file}".into()],
        env: HashMap::new(),
        interrupt_via_message: false,
        display_name,
        language: "python".into(),
        spec: None,
    }
}

fn from_spec(spec: zmq::KernelspecDir) -> KernelCommand {
    KernelCommand {
        argv: spec.kernelspec.argv,
        env: spec.kernelspec.env.unwrap_or_default(),
        interrupt_via_message: spec.kernelspec.interrupt_mode.as_deref() == Some("message"),
        display_name: spec.kernelspec.display_name,
        language: spec.kernelspec.language.to_ascii_lowercase(),
        spec: Some(spec.kernel_name),
    }
}

/// The active virtualenv's Python, if a virtualenv is active.
fn venv() -> Option<KernelCommand> {
    let venv = std::env::var("VIRTUAL_ENV").ok()?;
    let python = Path::new(&venv).join("bin/python");
    python.exists().then(|| launcher(python.to_string_lossy().into(), format!("Python ({venv})")))
}

/// The virtualenvs in the directory nbv was started from: each subdirectory with a
/// `pyvenv.cfg`, by name. The active virtualenv is left to `venv`.
fn local_venvs() -> Vec<KernelCommand> {
    let Ok(cwd) = std::env::current_dir() else { return vec![] };
    venvs_in(&cwd, std::env::var_os("VIRTUAL_ENV").map(PathBuf::from))
}

fn venvs_in(dir: &Path, active: Option<PathBuf>) -> Vec<KernelCommand> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|d| d.join("pyvenv.cfg").is_file() && d.join("bin/python").exists())
        .filter(|d| active.as_ref() != Some(d))
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|d| launcher(d.join("bin/python").to_string_lossy().into(), format!("Python ({})", d.display())))
        .collect()
}

/// `name` from `PATH`, if it is there.
fn on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.is_file())
}

/// `python3` from `PATH`, if there is one.
fn path_python() -> Option<KernelCommand> {
    let python = on_path("python3")?;
    Some(launcher(python.to_string_lossy().into(), format!("Python 3 ({})", python.display())))
}

/// Chooses the kernel for a notebook, as Jupyter picks one automatically, and preferring a
/// project's own virtualenv: for a Python notebook the active virtualenv first, then a
/// virtualenv in the working directory, then the notebook's kernelspec, then `python3` from
/// `PATH`. The choice is shown to the user, who can change it (`choices`).
pub async fn resolve(kernel_name: Option<&str>) -> Result<KernelCommand, KernelError> {
    let pythonish = kernel_name.is_none_or(|n| n.starts_with("python"));
    if pythonish && let Some(k) = venv().or_else(|| local_venvs().into_iter().next()) {
        return Ok(k);
    }
    let name = kernel_name.unwrap_or("python3");
    match zmq::find_kernelspec(name).await {
        Ok(spec) => Ok(from_spec(spec)),
        Err(e) => match path_python().filter(|_| pythonish) {
            Some(k) => Ok(k),
            None => Err(KernelError::NotFound(name.into(), e.to_string())),
        },
    }
}

/// Every kernel the user can pick: the active virtualenv, the virtualenvs in the working
/// directory, each installed kernelspec, and `python3` from `PATH`.
pub async fn choices() -> Vec<KernelCommand> {
    let mut specs = zmq::list_kernelspecs().await;
    specs.sort_by(|a, b| a.kernel_name.cmp(&b.kernel_name));
    venv().into_iter().chain(local_venvs()).chain(specs.into_iter().map(from_spec)).chain(path_python()).collect()
}

/// The Python a kernel runs ipykernel with, if it is started as `<python> -m ipykernel_launcher`.
fn ipykernel_python(cmd: &KernelCommand) -> Option<&str> {
    match cmd.argv.as_slice() {
        [python, m, module, ..] if m == "-m" && module == "ipykernel_launcher" => Some(python),
        _ => None,
    }
}

/// Whether `program args` exits successfully.
async fn succeeds(program: &str, args: &[&str], env: &HashMap<String, String>) -> Result<bool, KernelError> {
    let status = tokio::process::Command::new(program)
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(KernelError::Spawn)?;
    Ok(status.success())
}

/// The command that installs ipykernel for `cmd`, if `cmd` runs ipykernel with a Python that
/// lacks it: that Python's pip, or uv for a virtualenv without pip (as uv creates them).
pub async fn ipykernel_install(cmd: &KernelCommand) -> Result<Option<Vec<String>>, KernelError> {
    let Some(python) = ipykernel_python(cmd) else { return Ok(None) };
    if succeeds(python, &["-c", "import ipykernel"], &cmd.env).await? {
        return Ok(None);
    }
    if succeeds(python, &["-m", "pip", "--version"], &cmd.env).await? {
        return Ok(Some([python, "-m", "pip", "install", "ipykernel"].map(String::from).to_vec()));
    }
    let uv = on_path("uv").ok_or_else(|| {
        KernelError::Install(format!("ipykernel is missing, and there is neither pip in {python} nor uv on PATH"))
    })?;
    let uv = uv.to_string_lossy().into_owned();
    Ok(Some([uv.as_str(), "pip", "install", "--python", python, "ipykernel"].map(String::from).to_vec()))
}

/// Runs a command from `ipykernel_install`. A failure carries the tail of its output.
pub async fn install(argv: &[String]) -> Result<(), KernelError> {
    let (program, args) = argv.split_first().expect("an install command is never empty");
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| KernelError::Install(format!("cannot run {program}: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(20)..].join("\n");
    Err(KernelError::Install(format!("`{}` failed ({}):\n{tail}", argv.join(" "), out.status)))
}

/// A running kernel. Messages from every channel arrive on the sender given to `start`.
pub struct Kernel {
    pid: Option<u32>,
    /// Resolves once the process has exited.
    exited: tokio::sync::watch::Receiver<bool>,
    kill: Option<tokio::sync::oneshot::Sender<()>>,
    generation: u64,
    shell: zmq::DealerSendConnection,
    stdin: zmq::DealerSendConnection,
    control: zmq::ClientControlConnection,
    interrupt_via_message: bool,
    connection_file: PathBuf,
    tasks: Vec<JoinHandle<()>>,
}

static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl Kernel {
    /// Launches a kernel and waits until its iopub channel is live. `generation` tags death
    /// notices so a restarted session can ignore the previous process's.
    pub async fn start(
        cmd: &KernelCommand,
        cwd: &Path,
        generation: u64,
        tx: UnboundedSender<KernelMessage>,
    ) -> Result<Kernel, KernelError> {
        // Ports are free when picked, until the kernel binds them. Starting one kernel at a time
        // and holding the ports until the spawn keep this process from giving them out twice.
        let _one_at_a_time = STARTING.lock().await;
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (ports, listeners) = zmq::peek_ports_with_listeners(ip, 5).await.map_err(conn_err)?;
        let key: String = (0..32).map(|_| format!("{:x}", rand::random::<u8>() & 0xf)).collect();
        let info = ConnectionInfo {
            ip: ip.to_string(),
            transport: Transport::TCP,
            shell_port: ports[0],
            iopub_port: ports[1],
            stdin_port: ports[2],
            control_port: ports[3],
            hb_port: ports[4],
            key,
            signature_scheme: "hmac-sha256".into(),
            kernel_name: None,
        };
        let runtime = zmq::runtime_dir();
        std::fs::create_dir_all(&runtime).map_err(KernelError::Spawn)?;
        let session = format!("nbv-{:016x}", rand::random::<u64>());
        let connection_file = runtime.join(format!("kernel-{session}.json"));
        std::fs::write(&connection_file, serde_json::to_vec(&info).expect("serialisable"))
            .map_err(KernelError::Spawn)?;

        let Some(program) = cmd.argv.first() else {
            return Err(KernelError::NotFound(cmd.display_name.clone(), "empty argv".into()));
        };
        let file = connection_file.to_string_lossy().into_owned();
        let args = cmd.argv[1..].iter().map(|a| if a == "{connection_file}" { file.clone() } else { a.clone() });
        drop(listeners);
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .envs(&cmd.env)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(KernelError::Spawn)?;

        // Keep the tail of stderr for the death notice.
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::<String>::new()));
        let mut tasks = vec![];
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tasks.push(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut t = tail.lock().expect("not poisoned");
                    t.push_back(line);
                    if t.len() > 20 {
                        t.pop_front();
                    }
                }
            }));
        }
        let tail_text = || stderr_tail.lock().expect("not poisoned").iter().cloned().collect::<Vec<_>>().join("\n");

        let connect = async {
            let identity = zmq::peer_identity_for_session(&session).map_err(conn_err)?;
            let mut iopub = zmq::create_client_iopub_connection(&info, "", &session).await.map_err(conn_err)?;
            let shell = zmq::create_client_shell_connection_with_identity(&info, &session, identity.clone())
                .await
                .map_err(conn_err)?;
            let stdin =
                zmq::create_client_stdin_connection_with_identity(&info, &session, identity).await.map_err(conn_err)?;
            let control = zmq::create_client_control_connection(&info, &session).await.map_err(conn_err)?;
            let (mut shell_tx, mut shell_rx) = shell.split();
            // Probe until iopub delivers something: subscriptions are live once it does.
            loop {
                shell_tx.send(JupyterMessage::new(KernelInfoRequest {}, None)).await.map_err(conn_err)?;
                if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(300), iopub.read()).await {
                    break;
                }
            }
            // Drain the probes' replies before handing shell to the forwarder.
            while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(50), shell_rx.read()).await {}
            Ok::<_, KernelError>((iopub, shell_tx, shell_rx, stdin, control))
        };
        let limit = Duration::from_secs(60);
        let (mut iopub, shell_tx, mut shell_rx, stdin, control) = tokio::select! {
            r = tokio::time::timeout(limit, connect) => match r {
                Ok(r) => r?,
                Err(_) => return Err(KernelError::Timeout(limit, format!("\n{}", tail_text()))),
            },
            _ = child.wait() => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Err(KernelError::Connection(format!("kernel exited during startup\n{}", tail_text())));
            }
        };
        let (stdin_tx, mut stdin_rx) = stdin.split();

        let fwd = |tx: UnboundedSender<KernelMessage>| {
            move |m: JupyterMessage| tx.send(KernelMessage::Message(Box::new(m))).is_ok()
        };
        let send = fwd(tx.clone());
        tasks.push(tokio::spawn(async move {
            while let Ok(m) = iopub.read().await
                && send(m)
            {}
        }));
        let send = fwd(tx.clone());
        tasks.push(tokio::spawn(async move {
            while let Ok(m) = shell_rx.read().await
                && send(m)
            {}
        }));
        let send = fwd(tx.clone());
        tasks.push(tokio::spawn(async move {
            while let Ok(m) = stdin_rx.read().await
                && send(m)
            {}
        }));

        let mut kernel = Kernel {
            pid: child.id(),
            exited: tokio::sync::watch::channel(false).1,
            kill: None,
            generation,
            shell: shell_tx,
            stdin: stdin_tx,
            control,
            interrupt_via_message: cmd.interrupt_via_message,
            connection_file,
            tasks,
        };
        kernel.watch_exit(child, tx, stderr_tail);
        Ok(kernel)
    }

    /// Owns the child: reaps it, reports its death, and kills it on request.
    fn watch_exit(
        &mut self,
        mut child: Child,
        tx: UnboundedSender<KernelMessage>,
        tail: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    ) {
        let generation = self.generation;
        let (exited_tx, exited_rx) = tokio::sync::watch::channel(false);
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
        self.exited = exited_rx;
        self.kill = Some(kill_tx);
        self.tasks.push(tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = kill_rx => { let _ = child.kill().await; }
            }
            let _ = exited_tx.send(true);
            // Let the stderr reader catch up with the final lines.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stderr = tail.lock().expect("not poisoned").iter().cloned().collect::<Vec<_>>().join("\n");
            let _ = tx.send(KernelMessage::Died { generation, stderr });
        }));
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub async fn send_shell(&mut self, msg: JupyterMessage) -> Result<(), KernelError> {
        self.shell.send(msg).await.map_err(conn_err)
    }

    /// Answers an `input_request`.
    pub async fn reply_input(&mut self, request: &JupyterMessage, value: String) -> Result<(), KernelError> {
        let reply = InputReply { value, status: ReplyStatus::Ok, error: None };
        self.stdin.send(JupyterMessage::new(reply, Some(request))).await.map_err(conn_err)
    }

    pub async fn interrupt(&mut self) -> Result<(), KernelError> {
        if self.interrupt_via_message {
            return self.control.send(JupyterMessage::new(InterruptRequest {}, None)).await.map_err(conn_err);
        }
        if let Some(pid) = self.pid {
            // SAFETY: sending SIGINT to our own child process.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
        }
        Ok(())
    }

    /// Asks the kernel to shut down, then kills it if it does not exit promptly.
    pub async fn shutdown(mut self) {
        let _ = self.control.send(JupyterMessage::new(ShutdownRequest { restart: false }, None)).await;
        let mut exited = self.exited.clone();
        if tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|e| *e)).await.is_err()
            && let Some(kill) = self.kill.take()
        {
            let _ = kill.send(());
            let _ = tokio::time::timeout(Duration::from_secs(2), exited.wait_for(|e| *e)).await;
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        if let Some(pid) = self.pid
            && !*self.exited.borrow()
        {
            // SAFETY: killing our own child process, which has not been reaped yet.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        for t in &self.tasks {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.connection_file);
    }
}

/// Whether a message is an `input_request`, which the frontend answers via `reply_input`.
pub fn is_input_request(msg: &JupyterMessage) -> bool {
    matches!(msg.content, JupyterMessageContent::InputRequest(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venv(dir: &Path) {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("pyvenv.cfg"), "").unwrap();
        std::fs::write(dir.join("bin/python"), "").unwrap();
    }

    #[test]
    fn finds_virtualenvs_in_a_directory_except_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        venv(&dir.path().join(".venv"));
        venv(&dir.path().join("active"));
        std::fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        let found = venvs_in(dir.path(), Some(dir.path().join("active")));
        let pythons: Vec<&str> = found.iter().map(|k| k.argv[0].as_str()).collect();
        assert_eq!(pythons, [dir.path().join(".venv/bin/python").to_string_lossy()]);
        assert_eq!(ipykernel_python(&found[0]), Some(pythons[0]));
    }
}
