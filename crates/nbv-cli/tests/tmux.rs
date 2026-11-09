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
        Self::start_setup(cells, conf, false)
    }

    fn start_file_link(cells: &[&str]) -> Option<Session> {
        Self::start_setup(cells, "", true)
    }

    fn start_setup(cells: &[&str], conf: &str, file_link: bool) -> Option<Session> {
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
        if file_link {
            std::fs::create_dir(dir.path().join("data")).unwrap();
            std::fs::rename(dir.path().join("t.ipynb"), dir.path().join("data/real.ipynb")).unwrap();
            std::os::unix::fs::symlink("data/real.ipynb", dir.path().join("t.ipynb")).unwrap();
        }
        // The temporary directory's name is unique, so parallel tests get their own servers.
        let socket = format!("nbv-test-{}", dir.path().file_name().unwrap().to_string_lossy());
        let cmd = format!(
            "cd {0} && XDG_CONFIG_HOME={0}/config XDG_DATA_HOME={0}/data XDG_CACHE_HOME={0}/cache XDG_STATE_HOME={0}/state VIRTUAL_ENV={1} NBV_NVIM={2} {3} --clean t.ipynb; echo NVB-EXITED-$?; sleep 30",
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

    /// Abruptly replace the foreground application, then reopen the same path and state.
    /// This kills the process rather than sending Neovim a clean quit command.
    fn reopen_after_crash(&self) {
        self.reopen_with_init(None);
    }

    fn reopen_with_init(&self, init: Option<&str>) {
        self.reopen_named("t.ipynb", init);
    }

    fn reopen_named(&self, name: &str, init: Option<&str>) {
        let clean = if let Some(init) = init {
            let config = self.dir.path().join("config/nvim");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join("init.lua"), init).unwrap();
            ""
        } else {
            "--clean"
        };
        let cmd = format!(
            "cd {0} && XDG_CONFIG_HOME={0}/config XDG_DATA_HOME={0}/data XDG_CACHE_HOME={0}/cache XDG_STATE_HOME={0}/state VIRTUAL_ENV={1} NBV_NVIM={2} {3} {clean} {name}; echo NVB-EXITED-$?; sleep 30",
            self.dir.path().display(),
            root().join(".venv").display(),
            nvim().unwrap().display(),
            env!("CARGO_BIN_EXE_nvb"),
        );
        self.tmux(&["respawn-pane", "-k", "-t", "t", &cmd]);
    }

    fn recovery_manifest(&self) -> Option<serde_json::Value> {
        std::fs::read_dir(self.dir.path().join("state/nbv/recovery")).ok()?.filter_map(Result::ok).find_map(|e| {
            let bytes = std::fs::read(e.path().join("snapshot.json")).ok()?;
            serde_json::from_slice(&bytes).ok()
        })
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

    /// Waits until the screen satisfies `done`. A capture can land mid-redraw, so tests wait
    /// for the state they expect instead of asserting on one capture.
    fn wait_until(&self, what: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.screen();
            if done(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}\n{s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The screen once it has stopped changing, for checks that must hold of a whole frame.
    fn settled(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = self.screen();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let s = self.screen();
            if s == last {
                return s;
            }
            assert!(Instant::now() < deadline, "the screen never settled\n{s}");
            last = s;
        }
    }

    fn wait_for(&self, text: &str) -> String {
        self.wait_until(&format!("{text:?}"), |s| s.contains(text))
    }

    fn saved(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.dir.path().join("t.ipynb")).unwrap()).unwrap()
    }

    /// Waits until the notebook on disk satisfies `done`.
    fn wait_saved(&self, what: &str, done: impl Fn(&serde_json::Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(&self.saved()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}\n{}\n{}", self.saved(), self.screen());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Replaces cell `i`'s source on disk, as another program would: written aside, then
    /// renamed over the notebook.
    fn write_outside(&self, i: usize, source: &str) {
        let mut nb = self.saved();
        nb["cells"][i]["source"] = serde_json::json!(source);
        let aside = self.dir.path().join("t.ipynb.tmp");
        std::fs::write(&aside, serde_json::to_vec_pretty(&nb).unwrap()).unwrap();
        std::fs::rename(aside, self.dir.path().join("t.ipynb")).unwrap();
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
    // The outer terminal here is neither Kitty nor Ghostty, so images fall back to halfblocks.
    // Each cell is a box.
    s.wait_until("boxed cells", |s| s.contains('╭') && s.contains("│print(6 * 7)"));

    s.keys(&[":NvbRunAll", "Enter"]);
    // The output sits below its cell's box, outside it.
    s.wait_until("42 below the first box", |s| {
        let lines: Vec<&str> = s.lines().collect();
        s.contains("[1]") && lines.iter().position(|l| l.trim() == "42").is_some_and(|i| lines[i - 1].contains('╰'))
    });
    // The image is halfblock text, between its cell's box and the next cell's box.
    s.wait_until("an image below its box", |s| {
        let lines: Vec<&str> = s.lines().collect();
        lines.iter().position(|l| l.contains(['▀', '▄'])).is_some_and(|i| lines[i - 1].contains('╰'))
    });

    // input() is answered through Neovim's prompt, from navigation mode.
    s.wait_for("name? ");
    s.keys(&["nbv", "Enter"]);
    s.wait_for("[3]");

    // Scroll through the notebook: at every step the image rows are one contiguous block,
    // never drawn over a box.
    let full = count_blocks(&s.settled());
    s.keys(&["g", "g"]);
    for _ in 0..14 {
        let screen = s.settled();
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
    s.wait_until("a third cell, in edit", |s| s.contains(" EDIT") && s.matches('╭').count() == 3);
    s.keys(&["y = 3"]);
    // Esc leaves insert mode, then Esc in Normal mode leaves the cell.
    s.keys(&["Escape"]);
    s.keys(&["Escape"]);
    s.wait_for(" NAV");
    // o adds a cell and stays in navigation, so it repeats; Enter edits the selected one.
    s.keys(&["o", "o"]);
    s.wait_until("five cells, in navigation", |s| s.contains(" NAV") && s.matches('╭').count() == 5);
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
    // dd deletes the selected cell.
    s.wait_until("x + 1 deleted", |s| s.contains("y = 3") && !s.contains("x + 1"));
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
fn autosaves_and_follows_changes_on_disk() {
    let Some(s) = Session::start(&["x = 1", "y = 2"]) else { return };
    // Leaving Insert mode is enough: the notebook saves itself.
    s.keys(&["Enter"]);
    s.wait_for(" EDIT");
    s.keys(&["cc", "z = 3", "Escape"]);
    s.wait_saved("the edit saved", |nb| nb["cells"][0]["source"] == serde_json::json!("z = 3"));

    // Without unsaved changes, a change on disk shows at once, in the cell being edited too.
    s.write_outside(0, "w = 4");
    s.wait_until("w = 4 loaded, still editing", |s| s.contains("│w = 4") && s.contains(" EDIT"));

    // With unsaved changes, nbv asks once Insert mode is left. Loading discards them.
    s.keys(&["A", " + 1"]);
    s.write_outside(1, "v = 5");
    s.keys(&["Escape"]);
    s.wait_for("changed on disk, and you have unsaved changes");
    s.keys(&["1", "Enter"]);
    s.wait_until("theirs loaded", |s| s.contains("│w = 4 ") && !s.contains("w = 4 + 1") && s.contains("│v = 5"));

    // Keeping yours overwrites the file.
    s.keys(&["A", " # mine"]);
    s.write_outside(1, "u = 6");
    s.keys(&["Escape"]);
    s.wait_for("changed on disk, and you have unsaved changes");
    s.keys(&["2", "Enter"]);
    s.wait_saved("yours kept", |nb| {
        nb["cells"][0]["source"] == serde_json::json!("w = 4 # mine")
            && nb["cells"][1]["source"] == serde_json::json!("v = 5")
    });
    s.wait_for("│v = 5");
}

#[test]
fn slash_searches_the_whole_notebook() {
    let Some(s) = Session::start(&["alpha = 1", "beta = 2", "alpha2 = 3"]) else { return };
    // From the last cell, / wraps around to the first match; dd deletes the cell found.
    s.keys(&["G", "/", "alpha", "Enter"]);
    s.wait_for("search hit BOTTOM, continuing at TOP");
    s.keys(&["dd"]);
    s.wait_until("alpha = 1 deleted", |s| !s.contains("alpha = 1") && s.contains("│beta = 2"));
    // n goes on to the next cell with a match.
    s.keys(&["n"]);
    s.wait_for("/alpha");
    s.keys(&["dd"]);
    s.wait_until("alpha2 deleted", |s| !s.contains("alpha2") && s.contains("│beta = 2"));
}

#[test]
fn ctrl_e_and_ctrl_y_pan_line_by_line() {
    let cells: Vec<String> = (0..20).map(|i| format!("v{i} = {i}")).collect();
    let Some(s) = Session::start(&cells.iter().map(String::as_str).collect::<Vec<_>>()) else { return };
    s.wait_for("│v0 = 0");
    // One row down: v0's top border goes, its text stays, and so does the selection.
    s.keys(&["C-e"]);
    s.wait_until("one row panned", |s| s.lines().nth(1).is_some_and(|l| l.contains("│v0 = 0")));
    // Two more and v0 is gone: the selection moves to v1, the nearest cell in view.
    s.keys(&["2", "C-e"]);
    s.wait_until("v0 scrolled off", |s| !s.contains("v0 = 0") && s.contains("│v1 = 1"));
    s.keys(&["dd"]);
    s.wait_saved("v1 deleted", |nb| {
        let sources: Vec<&str> = nb["cells"].as_array().unwrap().iter().filter_map(|c| c["source"].as_str()).collect();
        sources.len() == 19 && sources[0] == "v0 = 0" && sources[1] == "v2 = 2"
    });
    s.keys(&["3", "C-y"]);
    s.wait_for("│v0 = 0");
}

#[test]
fn the_mouse_wheel_pans() {
    let cells: Vec<String> = (0..20).map(|i| format!("v{i} = {i}")).collect();
    let Some(s) = Session::start(&cells.iter().map(String::as_str).collect::<Vec<_>>()) else { return };
    s.wait_for("│v0 = 0");
    // A wheel notch down, as the terminal reports it (SGR mouse): three rows, past v0.
    s.keys(&["-l", "\x1b[<65;20;10M"]);
    s.wait_until("v0 scrolled off", |s| !s.contains("v0 = 0") && s.contains("│v1 = 1"));
    s.keys(&["-l", "\x1b[<64;20;10M"]);
    s.wait_for("│v0 = 0");
}

#[test]
fn help_and_the_header() {
    let Some(s) = Session::start(&["x = 1", "x + 1"]) else { return };
    // This tmux reports no modified keys, and nbv says so.
    s.wait_for("⚠ tmux: Shift/Ctrl+Enter off");
    // The way to the key list is always on screen, and ? opens it.
    s.wait_for("? help");
    s.keys(&["?"]);
    // The fix heads the list.
    s.wait_until("the key list", |s| {
        s.contains("nbv keys") && s.contains("set -s extended-keys-format csi-u") && s.contains("Cells (NAV)")
    });
    s.keys(&["q"]);
    s.wait_until("the key list closed", |s| s.contains(" NAV") && !s.contains("nbv keys"));

    // gg and G stay on cells; k past the first cell reaches the header, j comes back.
    s.keys(&["G", "g", "g"]);
    assert!(!s.settled().contains("h l select"));
    s.keys(&["k"]);
    s.wait_for("h l select");
    s.keys(&["j"]);
    s.wait_until("the cells' hint", |s| s.contains("? help") && !s.contains("h l select"));

    // The kernel item opens a list moved through with motions; q cancels it.
    s.keys(&["k", "l", "Enter"]);
    // The active virtualenv is offered.
    let venv = root().join(".venv");
    s.wait_until("the kernel list", |s| s.contains("<CR> picks · q cancels") && s.contains(venv.to_str().unwrap()));
    s.keys(&["j", "k", "q"]);
    s.wait_until("the kernel list closed", |s| s.contains(" NAV") && !s.contains("<CR> picks"));
    // <CR> picks the kernel under the cursor, and nbv restarts onto it.
    s.keys(&["Enter"]);
    s.wait_for("<CR> picks");
    s.keys(&["Enter"]);
    s.wait_until("the kernel picked", |s| s.contains(" NAV") && !s.contains("<CR> picks"));

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
    // <C-CR> stayed on the second cell.
    s.wait_until("[3] on the second cell", |s| s.contains("[1]") && s.contains("[3]") && !s.contains("[4]"));

    // From inside a cell, in Insert mode: <C-CR> runs what was just typed and keeps editing.
    s.keys(&["Enter", "A", " + 40"]);
    s.keys(&["C-Enter"]);
    s.wait_until("42, still editing", |s| s.contains("42") && s.contains(" EDIT"));
    // <S-CR> runs and moves on: past the last cell, into a new one.
    s.keys(&["S-Enter"]);
    s.wait_for("[5]");
    s.wait_until("a third cell, in edit", |s| s.contains(" EDIT") && s.matches('╭').count() == 3);
}

#[test]
fn survives_pane_splits_and_closes() {
    let cells = ["a = 1", "b = 2", "c = 3", "d = 4", "e = 5", "f = 6"];
    let Some(s) = Session::start(&cells) else { return };
    s.wait_for("f = 6");
    // Splitting shrinks nvb's pane before Neovim has resized; closing the new pane grows it back.
    s.tmux(&["split-window", "-d", "-v", "-t", "t", "sleep 60"]);
    s.tmux(&["split-window", "-d", "-h", "-t", "t", "sleep 60"]);
    s.wait_until("the first cell only", |s| s.contains("│a = 1") && !s.contains("│f = 6"));
    s.tmux(&["kill-pane", "-a", "-t", "t"]);
    s.wait_until("all six cells", |s| s.contains("│f = 6") && s.matches('╭').count() == 6);
}

#[test]
fn terminal_input_and_drawing_progress_during_a_kernel_flood() {
    let source =
        "import time\nend = time.monotonic() + 15\nwhile time.monotonic() < end:\n    print('flood-row', flush=True)";
    let Some(s) = Session::start(&[source, "target = 1"]) else { return };
    s.keys(&["r"]);
    s.wait_for("flood-row");
    let deadline = Instant::now() + Duration::from_secs(5);
    s.keys(&["j", "Enter", "i", "serviced_", "Escape"]);
    loop {
        let screen = s.screen();
        if screen.contains("serviced_target = 1") && screen.contains(" EDIT") {
            break;
        }
        assert!(Instant::now() < deadline, "terminal input/drawing starved during kernel flood\n{screen}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Saving is a synchronous ordered RPC; it must also complete during the flood.
    s.keys(&[":", "w", "Enter"]);
    loop {
        if s.saved()["cells"][1]["source"].as_str().is_some_and(|text| text.contains("serviced_target")) {
            break;
        }
        assert!(Instant::now() < deadline, "save RPC starved during kernel flood\n{}", s.screen());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn crash_during_insert_offers_explicit_recovery_on_reopen() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # unsaved insert"]);
    s.wait_until("Insert-mode recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # unsaved insert")
    });
    assert_eq!(s.saved()["cells"][0]["source"], "value = 1");
    s.reopen_after_crash();
    s.wait_until("file-associated recovery choice", |screen| {
        screen.contains("Recovery for") && screen.contains("t.ipynb") && screen.contains("Recover unsaved snapshot")
    });
    assert_eq!(s.saved()["cells"][0]["source"], "value = 1", "the prompt must not save anything");
    s.keys(&["1", "Enter"]);
    s.wait_for("│value = 1 # unsaved insert");
    s.wait_saved("recovered document saved", |nb| nb["cells"][0]["source"] == "value = 1 # unsaved insert");
    s.wait_until("successful save removed recovery", |_| s.recovery_manifest().is_none());
    s.keys(&[":q", "Enter"]);
    s.wait_for("NVB-EXITED-0");
    assert!(s.recovery_manifest().is_none());
}

#[test]
fn reopening_can_discard_recovery_and_cancel_keeps_it_available() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # discard me"]);
    s.wait_until("recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # discard me")
    });
    s.reopen_after_crash();
    s.wait_for("Recover unsaved snapshot");
    s.keys(&["Escape"]);
    s.wait_for("NVB-EXITED-0");
    assert!(s.recovery_manifest().is_some(), "cancelling must preserve recovery");
    s.reopen_after_crash();
    s.wait_for("Recover unsaved snapshot");
    s.keys(&["2", "Enter"]);
    s.wait_for("│value = 1");
    s.wait_until("discard removed recovery", |_| s.recovery_manifest().is_none());
    assert_eq!(s.saved()["cells"][0]["source"], "value = 1");
    s.keys(&[":q!", "Enter"]);
    s.wait_for("NVB-EXITED-0");
}

#[test]
fn clean_forced_quit_discards_insert_mode_recovery() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # intentionally discarded"]);
    s.wait_until("recovery persisted", |_| s.recovery_manifest().is_some());
    // Insert-mode Ctrl-o runs one command without becoming idle in Normal mode, so an
    // autosave tick cannot save the deliberately discarded text before :qa! is entered.
    // :qa! quits the home window as well as the cell's floating window.
    s.keys(&["C-o", ":qa!", "Enter"]);
    s.wait_for("NVB-EXITED-0");
    assert!(s.recovery_manifest().is_none());
    assert_eq!(s.saved()["cells"][0]["source"], "value = 1");
}

#[test]
fn recovered_edits_keep_external_change_protection_and_reload_cleanup() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # recovered"]);
    s.wait_until("recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # recovered")
    });
    s.write_outside(0, "external = 2");
    s.reopen_after_crash();
    s.wait_for("Recover unsaved snapshot");
    s.keys(&["1", "Enter"]);
    s.wait_for("changed on disk, and you have unsaved changes");
    assert_eq!(s.saved()["cells"][0]["source"], "external = 2");
    s.keys(&["Escape"]);
    s.wait_for(":w! keeps yours");
    s.keys(&[":e!", "Enter"]);
    s.wait_for("│external = 2");
    s.wait_until("external reload removed recovery", |_| s.recovery_manifest().is_none());
    assert_eq!(s.saved()["cells"][0]["source"], "external = 2");
}

#[test]
fn recovery_remains_available_when_the_disk_file_was_deleted() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # recovered"]);
    s.wait_until("recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # recovered")
    });
    std::fs::remove_file(s.dir.path().join("t.ipynb")).unwrap();
    s.reopen_after_crash();
    s.wait_for("Recover unsaved snapshot");
    assert!(!s.dir.path().join("t.ipynb").exists());
    s.keys(&["1", "Enter"]);
    s.wait_for("│value = 1 # recovered");
    assert!(!s.dir.path().join("t.ipynb").exists(), "ordinary autosave must not recreate a deleted file");
    s.keys(&[":w!", "Enter"]);
    s.wait_until("explicit force recreated the notebook", |_| s.dir.path().join("t.ipynb").exists());
    s.wait_saved("forced recovered save", |nb| nb["cells"][0]["source"] == "value = 1 # recovered");
    s.wait_until("forced save removed recovery", |_| s.recovery_manifest().is_none());
}

#[test]
fn corrupt_recovery_is_reported_with_both_file_paths() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # recovered"]);
    s.wait_until("recovery persisted", |_| s.recovery_manifest().is_some());
    let manifest = std::fs::read_dir(s.dir.path().join("state/nbv/recovery"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("snapshot.json"))
        .find(|p| p.exists())
        .unwrap();
    std::fs::write(&manifest, "broken").unwrap();
    s.reopen_after_crash();
    s.wait_until("explicit recovery corruption error", |screen| {
        screen.contains("recovery for")
            && screen.contains("t.ipynb")
            && screen.contains("snapshot.json")
            && screen.contains("NVB-EXITED-1")
    });
    assert_eq!(s.saved()["cells"][0]["source"], "value = 1");
}

#[test]
fn timer_writes_and_reloads_cannot_discard_an_unresolved_recovery_choice() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # pending"]);
    s.wait_until("recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # pending")
    });
    let before = s.recovery_manifest().unwrap();
    s.reopen_with_init(Some(r#"
        vim.ui.select = function(_, _, callback)
          _G.recovery_choice = callback
          vim.defer_fn(function()
            local write_ok, write_error = pcall(vim.cmd, 'write')
            local reload_ok, reload_error = pcall(vim.cmd, 'edit!')
            vim.fn.writefile({vim.json.encode({write_ok, tostring(write_error), reload_ok, tostring(reload_error)})}, 'guard-results.json')
          end, 50)
        end
    "#));
    let results = s.dir.path().join("guard-results.json");
    s.wait_until("timer write/reload rejected", |_| results.exists());
    let results: serde_json::Value = serde_json::from_slice(&std::fs::read(results).unwrap()).unwrap();
    assert_eq!(results[0], false);
    assert_eq!(results[2], false);
    assert!(results[1].as_str().unwrap().contains("recover or discard"), "{results}");
    assert!(results[3].as_str().unwrap().contains("recover or discard"), "{results}");
    assert_eq!(s.recovery_manifest().unwrap(), before);
    assert_eq!(s.saved()["cells"][0]["source"], "value = 1");
    s.reopen_after_crash();
    s.wait_for("Recover unsaved snapshot");
}

#[test]
fn deadly_editor_signals_keep_recovery_available() {
    for signal in ["-TERM", "-HUP"] {
        let Some(s) = Session::start(&["value = 1"]) else { return };
        s.wait_for("│value = 1");
        s.keys(&["Enter", "A", " # signal recovery"]);
        s.wait_until("recovery persisted", |_| {
            s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # signal recovery")
        });
        // Obtain the actual embedded Neovim pid while staying out of idle Normal mode.
        s.keys(&["C-o", ":call writefile([string(getpid())], 'nvim.pid')", "Enter"]);
        let pid_file = s.dir.path().join("nvim.pid");
        s.wait_until("editor pid written", |_| pid_file.exists());
        let pid = std::fs::read_to_string(pid_file).unwrap();
        assert!(Command::new("kill").args([signal, pid.trim()]).status().unwrap().success());
        s.wait_for("NVB-EXITED-");
        assert!(s.recovery_manifest().is_some(), "{signal} must preserve the durable snapshot");
        assert_eq!(s.saved()["cells"][0]["source"], "value = 1");
        s.reopen_after_crash();
        s.wait_for("Recover unsaved snapshot");
        s.keys(&["1", "Enter"]);
        s.wait_for("│value = 1 # signal recovery");
    }
}

#[test]
fn corrupt_disk_file_does_not_hide_valid_recovery_or_allow_discard_on_failed_reload() {
    let Some(s) = Session::start(&["value = 1"]) else { return };
    s.wait_for("│value = 1");
    s.keys(&["Enter", "A", " # valid snapshot"]);
    s.wait_until("recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # valid snapshot")
    });
    std::fs::write(s.dir.path().join("t.ipynb"), "broken notebook JSON").unwrap();
    s.reopen_after_crash();
    s.wait_for("Recover unsaved snapshot");
    s.keys(&["2", "Enter"]);
    s.wait_for("Recover unsaved snapshot");
    assert!(s.recovery_manifest().is_some(), "failed reload must keep recovery available");
    assert_eq!(std::fs::read_to_string(s.dir.path().join("t.ipynb")).unwrap(), "broken notebook JSON");
    s.keys(&["1", "Enter"]);
    s.wait_for("│value = 1 # valid snapshot");
}

#[test]
fn cross_directory_file_link_rename_preserves_target_and_recovers_through_the_new_alias() {
    let Some(s) = Session::start_file_link(&["value = 1"]) else { return };
    s.wait_for(" t.ipynb ");
    s.wait_for("│value = 1");
    let target = s.dir.path().join("data/real.ipynb");
    let original = std::fs::read(&target).unwrap();
    s.keys(&["Enter", "A", " # alias recovery"]);
    s.wait_until("alias recovery persisted", |_| {
        s.recovery_manifest().is_some_and(|m| m["cells"][0]["raw"]["source"] == "value = 1 # alias recovery")
    });
    // Keep the unsaved state while exercising the actual header rename picker. Recovery's
    // own timer remains active and independent of this test's disabled normal autosave.
    s.keys(&["C-o", ":lua require('nbv').autosave = function() end", "Enter"]);
    s.keys(&["Escape"]);
    s.keys(&["Escape"]);
    s.wait_for(" NAV");
    s.keys(&["k"]);
    s.wait_for("h l select");
    s.keys(&["Enter"]);
    s.wait_for("Rename: t.ipynb");
    s.keys(&["C-u", "renamed.ipynb", "Enter"]);
    s.wait_for(" renamed.ipynb ");
    let alias = s.dir.path().join("renamed.ipynb");
    assert!(!s.dir.path().join("t.ipynb").exists());
    assert!(std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read(&target).unwrap(),
        original,
        "renaming the link must leave its target in place and unsaved"
    );
    let manifest = s.recovery_manifest().unwrap();
    assert_eq!(manifest["notebook"], target.canonicalize().unwrap().to_string_lossy().as_ref());
    let second = Command::new(env!("CARGO_BIN_EXE_nvb"))
        .args(["--clean", "data/real.ipynb"])
        .current_dir(s.dir.path())
        .env("XDG_STATE_HOME", s.dir.path().join("state"))
        .env("NBV_NVIM", nvim().unwrap())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("cannot acquire recovery lock"));
    s.reopen_named("renamed.ipynb", None);
    s.wait_until("renamed alias recovery choice", |screen| {
        screen.contains("Recovery for") && screen.contains("renamed.ipynb")
    });
    s.keys(&["1", "Enter"]);
    s.wait_for(" renamed.ipynb ");
    s.wait_for("│value = 1 # alias recovery");
    s.wait_until("recovered source saved through alias", |_| {
        std::fs::read(&target)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|nb| nb["cells"][0]["source"] == "value = 1 # alias recovery")
    });
    assert!(std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink());
    s.wait_until("alias recovery cleaned after save", |_| s.recovery_manifest().is_none());
}
