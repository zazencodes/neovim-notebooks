//! The second integration harness (§16.6): the real `nvb` binary in a detached tmux session,
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

    /// Starts nvb on a notebook written from `cells` (source strings), under a tmux with no
    /// configuration.
    fn start(cells: &[&str]) -> Option<Session> {
        Session::start_with(cells, "")
    }

    /// As `start`, under a tmux configured with `conf`.
    fn start_with(cells: &[&str], conf: &str) -> Option<Session> {
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
            "cd {0} && XDG_STATE_HOME={0}/state VIRTUAL_ENV={1} NBV_NVIM={2} {3} --clean t.ipynb; echo NVB-EXITED-$?; sleep 30",
            dir.path().display(),
            venv.display(),
            nvim.display(),
            env!("CARGO_BIN_EXE_nvb"),
        );
        let s = Session { socket, dir };
        let conf_path = s.dir.path().join("tmux.conf");
        std::fs::write(&conf_path, conf).unwrap();
        // Wide enough that the header's hint fits beside a kernel named after a long venv path.
        s.tmux(&["-f", &conf_path.to_string_lossy(), "new-session", "-d", "-s", "t", "-x", "200", "-y", "30", &cmd]);
        s.wait_for(" NAV");
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

    fn wait_for_gone(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.screen();
            if !s.contains(text) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {text:?} to go\n{s}");
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
fn run_image_input_edit_and_save_inside_tmux() {
    let Some(s) = Session::start(&["print(6 * 7)", IMAGE, "input('name? ')"]) else {
        eprintln!("skipped: needs tmux, Neovim 0.12 and .venv");
        return;
    };
    // tmux here has no allow-passthrough, so images fall back to halfblocks.
    let screen = s.wait_for("print(6 * 7)");
    assert!(screen.contains("╭") && screen.contains("│print(6 * 7)"), "each cell is a box\n{screen}");

    s.keys(&[":NvbRunAll", "Enter"]);
    let screen = s.wait_for("42");
    assert!(screen.contains("[1]"), "{screen}");
    // The output sits below its cell's box, outside it.
    let lines: Vec<&str> = screen.lines().collect();
    let out = lines.iter().position(|l| l.trim() == "42").unwrap();
    assert!(lines[out - 1].contains("╰"), "{screen}");
    // The image is halfblock text, between its cell's box and the next cell's box.
    let screen = s.wait_for_any(&['▀', '▄']);
    let lines: Vec<&str> = screen.lines().collect();
    let first_block = lines.iter().position(|l| l.contains(['▀', '▄'])).unwrap();
    assert!(lines[first_block - 1].contains("╰"), "{screen}");

    // input() is answered through Neovim's prompt, from navigation mode.
    s.wait_for("name? ");
    s.keys(&["nbv", "Enter"]);
    s.wait_for("[3]");

    // Scroll through the notebook: at every step the image rows are one contiguous block,
    // never drawn over a box.
    let full = count_blocks(&s.screen());
    s.keys(&["g", "g"]);
    for _ in 0..14 {
        let screen = s.screen();
        let lines: Vec<&str> = screen.lines().collect();
        let rows: Vec<usize> = (0..lines.len()).filter(|i| lines[*i].contains(['▀', '▄'])).collect();
        if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
            assert_eq!(last - first + 1, rows.len(), "image rows not contiguous\n{screen}");
            assert!(rows.len() <= full, "{screen}");
            assert!(rows.iter().all(|r| !lines[*r].contains('│')), "image inside a box\n{screen}");
        }
        s.keys(&["C-e"]);
    }

    // Enter edits the selected cell in its own Neovim window; :q returns to navigation.
    s.keys(&["g", "g", "Enter"]);
    s.wait_for(" EDIT");
    s.keys(&["A", "  # edited", "Escape"]);
    s.wait_for("print(6 * 7)  # edited");
    s.keys(&[":q", "Enter"]);
    s.wait_for(" NAV");

    s.keys(&[":wq", "Enter"]);
    s.wait_for("NVB-EXITED-0");
    let saved = s.saved();
    // The cell keeps the string form it was loaded with.
    assert_eq!(saved["cells"][0]["source"], serde_json::json!("print(6 * 7)  # edited"));
    assert_eq!(saved["cells"][0]["outputs"][0]["text"], serde_json::json!(["42\n"]));
    assert!(saved["cells"][1]["outputs"][0]["data"]["image/png"].is_string());
    assert_eq!(saved["cells"][2]["execution_count"], 3);
}

#[test]
fn run_advance_and_leave_cells() {
    let Some(s) = Session::start(&["x = 1", "x + 1"]) else { return };
    // x runs the selected cell and selects the next.
    s.keys(&["x"]);
    s.wait_for("[1]");
    s.keys(&["x"]);
    s.wait_for("[2]");
    // Past the last cell, a new one opens in insert mode.
    let screen = s.wait_for(" EDIT");
    assert_eq!(screen.matches('╭').count(), 3, "{screen}");
    s.keys(&["y = 3"]);
    // Esc leaves insert mode, then Esc in Normal mode leaves the cell.
    s.keys(&["Escape"]);
    s.keys(&["Escape"]);
    s.wait_for(" NAV");
    // o adds a cell and stays in navigation, so it repeats; Enter edits the selected one.
    s.keys(&["o", "o"]);
    let screen = s.screen();
    assert_eq!(screen.matches('╭').count(), 5, "{screen}");
    assert!(screen.contains(" NAV"), "{screen}");
    s.keys(&["Enter"]);
    s.wait_for(" EDIT");
    // <C-c> leaves Insert mode, then <C-c> in Normal mode leaves the cell.
    // Neovim drops typeahead on <C-c>, so each key waits for the one before it.
    s.keys(&["i", "z = 4"]);
    s.wait_for("z = 4");
    s.keys(&["C-c"]);
    s.keys(&["C-c"]);
    s.wait_for(" NAV");
    s.keys(&["k", "k", "k", "d", "d"]);
    let screen = s.wait_for("y = 3");
    assert!(!screen.contains("x + 1"), "dd deleted the selected cell\n{screen}");
    s.keys(&["u"]);
    s.wait_for("x + 1");
    s.keys(&[":wq", "Enter"]);
    s.wait_for("NVB-EXITED-0");
    let sources: Vec<serde_json::Value> =
        s.saved()["cells"].as_array().unwrap().iter().map(|c| c["source"].clone()).collect();
    assert_eq!(
        sources,
        [
            serde_json::json!("x = 1"),
            serde_json::json!("x + 1"),
            serde_json::json!(["y = 3"]),
            serde_json::json!([]),
            serde_json::json!(["z = 4"]),
        ]
    );
}

#[test]
fn clean_exit_leaves_no_processes() {
    let Some(s) = Session::start(&["x = 1"]) else { return };
    // r runs the cell and stays on it.
    s.keys(&["r"]);
    s.wait_for("[1]");
    // Outputs are unsaved changes: :q refuses, :q! quits.
    s.keys(&[":q", "Enter"]);
    s.wait_for("E37");
    s.keys(&[":q!", "Enter"]);
    s.wait_for("NVB-EXITED-0");
    let ps = Command::new("ps").args(["-eo", "command"]).output().unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    let dir = s.dir.path().to_string_lossy().into_owned();
    // The pane's own shell names the directory too; bash, unlike zsh, outlives its last command.
    let left = |l: &&str| l.contains(&dir) && !l.contains("tmux") && !l.contains("NVB-EXITED");
    assert!(
        !ps.lines().any(|l| left(&l)),
        "processes left behind:\n{}",
        ps.lines().filter(left).collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn help_and_the_header() {
    let Some(s) = Session::start(&["x = 1", "x + 1"]) else { return };
    // This tmux reports no modified keys, and nbv says so.
    s.wait_for("⚠ tmux: Shift/Ctrl+Enter off");
    // The way to the key list is always on screen, and ? opens it.
    s.wait_for("? help");
    s.keys(&["?"]);
    let screen = s.wait_for("nbv keys");
    assert!(screen.contains("set -s extended-keys-format csi-u"), "the fix heads the list\n{screen}");
    assert!(screen.contains("Cells (NAV)"), "{screen}");
    s.keys(&["q"]);
    s.wait_for(" NAV");
    assert!(!s.screen().contains("nbv keys"));

    // gg and G stay on cells; k past the first cell reaches the header, j comes back.
    s.keys(&["G", "g", "g"]);
    assert!(!s.screen().contains("h l select"));
    s.keys(&["k"]);
    s.wait_for("h l select");
    s.keys(&["j"]);
    s.wait_for("? help");
    assert!(!s.screen().contains("h l select"));

    // The kernel item opens a list moved through with motions; q cancels it.
    s.keys(&["k", "l", "Enter"]);
    let screen = s.wait_for("<CR> picks · q cancels");
    assert!(screen.contains("neovim-notebooks/.venv"), "the active virtualenv is offered\n{screen}");
    s.keys(&["j", "k", "q"]);
    s.wait_for(" NAV");
    assert!(!s.screen().contains("<CR> picks"));
    // <CR> picks the kernel under the cursor, and nbv restarts onto it.
    s.keys(&["Enter"]);
    s.wait_for("<CR> picks");
    s.keys(&["Enter"]);
    s.wait_for(" NAV");
    assert!(!s.screen().contains("<CR> picks"));

    // The file name item renames the notebook; later writes follow it.
    s.keys(&["h", "Enter"]);
    s.wait_for("Rename: t.ipynb");
    s.keys(&["C-u", "u.ipynb", "Enter"]);
    s.wait_for(" u.ipynb ");
    s.keys(&["j", "Enter"]);
    s.wait_for(" EDIT");
    s.keys(&["A", "  # edited", "Escape"]);
    s.keys(&["Escape"]);
    s.wait_for(" NAV");
    s.keys(&[":wq", "Enter"]);
    s.wait_for("NVB-EXITED-0");
    assert!(!s.dir.path().join("t.ipynb").exists());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.dir.path().join("u.ipynb")).unwrap()).unwrap();
    assert_eq!(saved["cells"][0]["source"], serde_json::json!("x = 1  # edited"));
    // The kernel picked is remembered for the notebook, under its new name.
    let memory: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.dir.path().join("state/nbv/kernels.json")).unwrap()).unwrap();
    let key = s.dir.path().canonicalize().unwrap().join("u.ipynb").to_string_lossy().into_owned();
    assert!(memory[&key]["display_name"].is_string(), "{memory}");
}

#[test]
fn shift_and_ctrl_enter_run_cells() {
    let conf = "set -s extended-keys on\nset -s extended-keys-format csi-u\n";
    let Some(s) = Session::start_with(&["x = 1", "x + 1"], conf) else { return };
    // From navigation: <S-CR> runs and selects the next cell, <C-CR> runs and stays.
    s.keys(&["S-Enter"]);
    s.wait_for("[1]");
    s.keys(&["C-Enter"]);
    s.wait_for("[2]");
    s.keys(&["C-Enter"]);
    let screen = s.wait_for("[3]");
    assert!(screen.contains("[1]") && !screen.contains("[4]"), "<C-CR> stayed on the second cell\n{screen}");

    // From inside a cell, in Insert mode: <C-CR> runs what was just typed and keeps editing.
    s.keys(&["Enter", "A", " + 40"]);
    s.keys(&["C-Enter"]);
    let screen = s.wait_for("42");
    assert!(screen.contains(" EDIT"), "{screen}");
    // <S-CR> runs and moves on: past the last cell, into a new one.
    s.keys(&["S-Enter"]);
    s.wait_for("[5]");
    let screen = s.wait_for(" EDIT");
    assert_eq!(screen.matches('╭').count(), 3, "{screen}");
}

#[test]
fn survives_pane_splits_and_closes() {
    let cells = ["a = 1", "b = 2", "c = 3", "d = 4", "e = 5", "f = 6"];
    let Some(s) = Session::start(&cells) else { return };
    s.wait_for("f = 6");
    // Splitting shrinks nvb's pane before Neovim has resized; closing the new pane grows it back.
    s.tmux(&["split-window", "-d", "-v", "-t", "t", "sleep 60"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "t", "sleep 60"]);
    let screen = s.wait_for_gone("│f = 6");
    assert!(screen.contains("│a = 1"), "{screen}");
    s.tmux(&["kill-pane", "-a", "-t", "t"]);
    let screen = s.wait_for("│f = 6");
    assert_eq!(screen.matches('╭').count(), 6, "{screen}");
}
