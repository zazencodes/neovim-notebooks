//! Spike 5: the executor against a real `ipykernel`. Skipped when no Python with ipykernel is
//! available; set `NBV_TEST_PYTHON` or create `.venv` at the repository root.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nbv_core::exec::{ExecEvent, Executor};
use nbv_core::kernel::{Kernel, KernelCommand, KernelMessage, is_input_request};
use nbv_core::{CellKey, ExecState, Notebook};
use tokio::sync::mpsc;

fn python() -> Option<PathBuf> {
    let p = std::env::var_os("NBV_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.venv/bin/python"));
    p.exists().then_some(p)
}

fn command(python: &Path) -> KernelCommand {
    KernelCommand {
        argv: vec![
            python.to_string_lossy().into(),
            "-m".into(),
            "ipykernel_launcher".into(),
            "-f".into(),
            "{connection_file}".into(),
        ],
        env: Default::default(),
        interrupt_via_message: false,
        display_name: "test".into(),
        language: "python".into(),
        spec: None,
    }
}

fn notebook(sources: &[&str]) -> Notebook {
    let cells: Vec<serde_json::Value> = sources
        .iter()
        .map(|s| serde_json::json!({"cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": s}))
        .collect();
    let json = serde_json::json!({"cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5});
    Notebook::from_bytes(Path::new("k.ipynb"), &serde_json::to_vec(&json).unwrap(), Default::default()).unwrap()
}

struct Session {
    nb: Notebook,
    ex: Executor,
    kernel: Kernel,
    rx: mpsc::UnboundedReceiver<KernelMessage>,
}

impl Session {
    async fn start(sources: &[&str]) -> Option<Session> {
        let python = python()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let kernel = Kernel::start(&command(&python), Path::new("."), 1, tx).await.unwrap();
        Some(Session { nb: notebook(sources), ex: Executor::default(), kernel, rx })
    }

    fn key(&self, i: usize) -> CellKey {
        self.nb.order()[i].clone()
    }

    async fn run(&mut self, i: usize) {
        let key = self.key(i);
        let msg = self.ex.request(&mut self.nb, &key).unwrap();
        self.kernel.send_shell(msg).await.unwrap();
    }

    /// Pumps messages until `done` holds, answering input requests with `input`.
    async fn pump_until(&mut self, input: Option<&str>, done: impl Fn(&Notebook) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done(&self.nb) {
            let msg = tokio::time::timeout_at(deadline, self.rx.recv()).await.expect("timed out").unwrap();
            let KernelMessage::Message(m) = msg else { panic!("kernel died: {msg:?}") };
            for e in self.ex.handle(&mut self.nb, &m) {
                if let ExecEvent::InputRequested { .. } = e {
                    assert!(is_input_request(&m));
                    self.kernel.reply_input(&m, input.expect("unexpected input()").into()).await.unwrap();
                }
            }
        }
    }
}

fn text(nb: &Notebook, key: &CellKey) -> String {
    serde_json::to_string(nb.cell(key).unwrap().outputs()).unwrap()
}

#[tokio::test]
async fn executes_streams_results_errors_and_input() {
    let Some(mut s) = Session::start(&[
        "print('hello')\n6 * 7",
        "1/0",
        "name = input('who? ')\nprint('hi', name)",
        "from IPython.display import display, update_display\nh = display('a', display_id=True)\nh.update('b')",
    ])
    .await
    else {
        eprintln!("skipped: no python with ipykernel");
        return;
    };

    s.run(0).await;
    let (k0, k1, k2, k3) = (s.key(0), s.key(1), s.key(2), s.key(3));
    s.pump_until(None, |nb| matches!(nb.cell(&k0).unwrap().runtime.exec, ExecState::Ok)).await;
    let out = text(&s.nb, &k0);
    assert!(out.contains("hello") && out.contains("42"), "{out}");
    assert_eq!(s.nb.cell(&k0).unwrap().execution_count(), Some(1));

    s.run(1).await;
    let k = k1.clone();
    s.pump_until(None, move |nb| nb.cell(&k).unwrap().runtime.exec == ExecState::Error).await;
    assert_eq!(s.nb.cell(&k1).unwrap().runtime.exec, ExecState::Error);
    assert!(text(&s.nb, &k1).contains("ZeroDivisionError"));

    s.run(2).await;
    let k = k2.clone();
    s.pump_until(Some("nbv"), move |nb| matches!(nb.cell(&k).unwrap().runtime.exec, ExecState::Ok)).await;
    assert!(text(&s.nb, &k2).contains("hi nbv"), "{}", text(&s.nb, &k2));

    s.run(3).await;
    let k = k3.clone();
    s.pump_until(None, move |nb| matches!(nb.cell(&k).unwrap().runtime.exec, ExecState::Ok)).await;
    let out = s.nb.cell(&k3).unwrap().outputs().to_vec();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0]["data"]["text/plain"], serde_json::json!(["'b'"]));

    s.kernel.shutdown().await;
}

#[tokio::test]
async fn interrupt_stops_a_running_cell() {
    let Some(mut s) = Session::start(&["import time\ntime.sleep(60)"]).await else { return };
    s.run(0).await;
    let k = s.key(0);
    let k2 = k.clone();
    s.pump_until(None, move |nb| nb.cell(&k2).unwrap().runtime.exec == ExecState::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.kernel.interrupt().await.unwrap();
    s.pump_until(None, |nb| nb.cell(&k).unwrap().runtime.exec == ExecState::Error).await;
    assert!(text(&s.nb, &k).contains("KeyboardInterrupt"));
    s.kernel.shutdown().await;
}

#[tokio::test]
async fn death_is_reported() {
    let Some(mut s) = Session::start(&["import os\nos._exit(3)"]).await else { return };
    s.run(0).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout_at(deadline, s.rx.recv()).await.expect("timed out").unwrap() {
            KernelMessage::Died { generation, .. } => {
                assert_eq!(generation, 1);
                break;
            }
            KernelMessage::Message(m) => {
                s.ex.handle(&mut s.nb, &m);
            }
        }
    }
}
