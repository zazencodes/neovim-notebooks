//! The second integration harness (§16.6): the real `nbv` binary in a detached tmux session,
//! driven with `send-keys` and checked with `capture-pane`. Halfblock images are text in tmux's
//! grid, so image placement and clipping are checked here too. Skipped without tmux, Neovim
//! 0.12 (`NBV_NVIM` or `.tools/`), or a Python with ipykernel (`.venv/`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn nvim() -> Option<PathBuf> {
    let p = std::env::var_os("NBV_NVIM")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join(".tools/nvim-macos-arm64/bin/nvim"));
    p.exists().then_some(p)
}

struct Session {
    socket: String,
    dir: tempfile::TempDir,
}

impl Session {
    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux").arg("-L").arg(&self.socket).args(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Starts nbv on a notebook written from `cells` (source strings).
    fn start(cells: &[&str]) -> Option<Session> {
        let nvim = nvim()?;
        let venv = root().join(".venv");
        if !venv.join("bin/python").exists() || Command::new("tmux").arg("-V").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let cells: Vec<serde_json::Value> = cells
            .iter()
            .enumerate()
            .map(|(i, s)| {
                serde_json::json!({"cell_type": "code", "execution_count": null, "id": format!("c{i}"),
                    "metadata": {}, "outputs": [], "source": s})
            })
            .collect();
        let nb = serde_json::json!({"cells": cells, "metadata": {"kernelspec": {"name": "python3", "display_name": "Python 3"}},
            "nbformat": 4, "nbformat_minor": 5});
        std::fs::write(dir.path().join("t.ipynb"), serde_json::to_vec_pretty(&nb).unwrap()).unwrap();
        // The temporary directory's name is unique, so parallel tests get their own servers.
        let socket = format!("nbv-test-{}", dir.path().file_name().unwrap().to_string_lossy());
        let cmd = format!(
            "cd {} && VIRTUAL_ENV={} NBV_NVIM={} {} --clean t.ipynb; echo NBV-EXITED-$?; sleep 30",
            dir.path().display(),
            venv.display(),
            nvim.display(),
            env!("CARGO_BIN_EXE_nbv"),
        );
        let s = Session { socket, dir };
        s.tmux(&["-f", "/dev/null", "new-session", "-d", "-s", "t", "-x", "90", "-y", "30", &cmd]);
        s.wait_for("# %% id=\"c0\"");
        Some(s)
    }

    fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "t"])
    }

    fn keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "t"];
        args.extend_from_slice(keys);
        self.tmux(&args);
        std::thread::sleep(Duration::from_millis(150));
    }

    fn wait_for(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.screen();
            if s.contains(text) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {text:?}\n{s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for_any(&self, chars: &[char]) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.screen();
            if s.contains(chars) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {chars:?}\n{s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.dir.path().join("t.ipynb")).unwrap()).unwrap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
    }
}

fn count_blocks(screen: &str) -> usize {
    screen.lines().filter(|l| l.contains(['▀', '▄'])).count()
}

const IMAGE: &str = r#"import zlib, struct
from IPython.display import Image, display
def png(w, h):
    rows = b"".join(b"\x00" + b"".join(bytes([x * 255 // w, y * 255 // h, 160]) for x in range(w)) for y in range(h))
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
display(Image(png(80, 60)))"#;

#[test]
fn edit_run_image_and_save_inside_tmux() {
    let Some(s) = Session::start(&["print(6 * 7)", IMAGE, "input('name? ')"]) else {
        eprintln!("skipped: needs tmux, Neovim 0.12 and .venv");
        return;
    };
    // tmux here has no allow-passthrough: nbv says so, and falls back to halfblocks.
    s.wait_for("allow-passthrough");

    s.keys(&[":NbvRunAll", "Enter"]);
    let screen = s.wait_for("42");
    assert!(screen.contains("[1] ✓"), "{screen}");
    // The image is halfblock text, placed between its cell and the next marker.
    let screen = s.wait_for_any(&['▀', '▄']);
    let lines: Vec<&str> = screen.lines().collect();
    let first_block = lines.iter().position(|l| l.contains('▀') || l.contains('▄')).unwrap();
    assert!(lines[first_block - 1].starts_with("display(Image"), "{screen}");
    let after = lines[first_block..].iter().position(|l| !(l.contains('▀') || l.contains('▄'))).unwrap();
    assert!(lines[first_block + after].starts_with("# %% id=\"c2\""), "{screen}");

    // input() is answered through Neovim's prompt.
    s.wait_for("name? ");
    s.keys(&["nbv", "Enter"]);
    s.wait_for("[3] ✓");

    // Scroll through the notebook: at every step the image rows are one contiguous block,
    // and while the image's bottom is on screen the next marker follows it directly.
    let full = count_blocks(&s.screen());
    s.keys(&["gg"]);
    for _ in 0..14 {
        let screen = s.screen();
        let lines: Vec<&str> = screen.lines().collect();
        let rows: Vec<usize> = (0..lines.len()).filter(|i| lines[*i].contains(['▀', '▄'])).collect();
        if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
            assert_eq!(last - first + 1, rows.len(), "image rows not contiguous\n{screen}");
            assert!(rows.len() <= full, "{screen}");
            if last + 1 < lines.len() && !lines[last + 1].contains("t.ipynb.py") {
                assert!(lines[last + 1].starts_with("# %% id=\"c2\""), "{screen}");
            }
        }
        s.keys(&["C-e"]);
    }

    s.keys(&[":wq", "Enter"]);
    s.wait_for("NBV-EXITED-0");
    let saved = s.saved();
    assert_eq!(saved["cells"][0]["outputs"][0]["text"], serde_json::json!(["42\n"]));
    assert!(saved["cells"][1]["outputs"][0]["data"]["image/png"].is_string());
    assert_eq!(saved["cells"][2]["execution_count"], 3);
}

#[test]
fn clean_exit_leaves_no_processes() {
    let Some(s) = Session::start(&["x = 1"]) else { return };
    s.keys(&[":NbvRun", "Enter"]);
    s.wait_for("[1] ✓");
    s.keys(&[":q", "Enter"]);
    s.wait_for("NBV-EXITED-0");
    let ps = Command::new("ps").args(["-eo", "command"]).output().unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    let dir = s.dir.path().to_string_lossy().into_owned();
    assert!(!ps.lines().any(|l| l.contains(&dir) && !l.contains("tmux")), "processes left behind:\n{ps}");
}
