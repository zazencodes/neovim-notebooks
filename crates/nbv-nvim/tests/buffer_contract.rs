//! The cell buffer contract (§7, §9) through a real Neovim: every cell edits in its own
//! buffer and window, writes commit the notebook, and the home window is transparent.

use nbv_core::{CellKey, Change};
use nbv_nvim::editor::Focus;
use nbv_nvim::harness::{Harness, Options, repo_root};
use serde_json::{Value, json};

fn ids(saved: &Value) -> Vec<Option<String>> {
    saved["cells"].as_array().unwrap().iter().map(|c| c["id"].as_str().map(str::to_string)).collect()
}

/// Buffer options of the current window's buffer.
async fn current(h: &Harness) -> Value {
    let v = h
        .lua(
            "return { vim.bo.filetype, vim.bo.buftype, vim.bo.swapfile, vim.bo.modified, \
             vim.api.nvim_buf_get_name(0), vim.b.nbv_key or '', vim.api.nvim_win_get_config(0).relative }",
        )
        .await;
    serde_json::to_value(
        v.as_array().unwrap().iter().map(|x| x.to_string().trim_matches('"').to_string()).collect::<Vec<_>>(),
    )
    .unwrap()
}

#[tokio::test]
async fn each_cell_is_its_own_ordinary_buffer() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    let info = current(&h).await;
    assert_eq!(info[0], "python");
    assert_eq!(info[1], "", "buftype is empty, so LSP attaches");
    assert_eq!(info[2], "false");
    assert_eq!(info[3], "false");
    assert!(info[4].as_str().unwrap().ends_with("v4.5-outputs.ipynb.a1b2c3d4.py"), "{info}");
    assert_eq!(info[5], "a1b2c3d4");
    assert_eq!(info[6], "editor", "a floating window");
    assert_eq!(h.lua("return vim.api.nvim_buf_line_count(0)").await.as_u64(), Some(2));

    let md = h.key(0);
    h.enter(&md, false).await;
    let info = current(&h).await;
    assert_eq!(info[0], "markdown");
    assert!(info[4].as_str().unwrap().ends_with(".e5f6a7b8.md"));
    // The initial text is not an undoable change.
    h.keys("u").await;
    assert_eq!(h.lua("return vim.api.nvim_get_current_line()").await.as_str(), Some("## Results"));
}

#[tokio::test]
async fn motions_and_edits_stay_inside_the_cell() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    // G, dG, and gg=G reach only this cell's two lines.
    h.keys("GoX = 1<Esc>").await;
    assert_eq!(h.source(1), "import numpy as np\nnp.arange(3)\nX = 1");
    h.keys("ggdG").await;
    assert_eq!(h.source(1), "");
    assert_eq!(h.source(0), "## Results", "other cells are untouched");
    assert_eq!(h.source(2), "plot()");
    h.keys("u").await;
    assert_eq!(h.source(1), "import numpy as np\nnp.arange(3)\nX = 1");
    let outputs = h.state.lock().unwrap().nb.cell(&code).unwrap().outputs().len();
    assert_eq!(outputs, 1, "editing keeps outputs");
}

#[tokio::test]
async fn write_from_a_cell_commits_the_notebook_and_nothing_else() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    h.keys("AX<Esc>").await;
    assert!(h.lua("return vim.bo.modified").await.as_bool().unwrap());
    h.cmd("w").await;
    assert!(!h.lua("return vim.bo.modified").await.as_bool().unwrap());
    let saved = h.saved();
    assert_eq!(saved["cells"][1]["source"], json!(["import numpy as npX\n", "np.arange(3)"]));
    assert_eq!(saved["cells"][1]["outputs"][0]["data"]["text/plain"], json!(["42"]));
    let names: Vec<_> = std::fs::read_dir(h.dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 1, "only the notebook exists: {names:?}");
    assert!(h.try_cmd("w other.py").await.is_err(), "writing a cell elsewhere is refused");
}

#[tokio::test]
async fn legacy_notebook_saves_without_ids_until_edited() {
    let h = Harness::open("legacy-4.4-outputs.ipynb", Options::default()).await;
    h.cmd("w").await;
    let saved = h.saved();
    assert_eq!(saved["nbformat_minor"], 4);
    assert!(ids(&saved).iter().all(Option::is_none));

    let first = h.key(0);
    h.enter(&first, false).await;
    h.keys("o# more<Esc>").await;
    h.cmd("w").await;
    let saved = h.saved();
    assert_eq!(saved["nbformat_minor"], 5);
    let keys: Vec<String> = h.state.lock().unwrap().nb.order().iter().map(|k| k.to_string()).collect();
    assert_eq!(ids(&saved), keys.into_iter().map(Some).collect::<Vec<_>>());
}

#[tokio::test]
async fn format_on_save_formats_the_cell_before_commit() {
    let ruff = repo_root().join(".venv/bin/ruff");
    if !ruff.exists() {
        eprintln!("skipped: no ruff");
        return;
    }
    let init = format!(
        r#"
        vim.api.nvim_create_autocmd('BufWritePre', {{
          pattern = '*.py',
          callback = function(a)
            local view = vim.fn.winsaveview()
            vim.cmd('silent %!{} format -')
            vim.fn.winrestview(view)
          end,
        }})
        "#,
        ruff.display()
    );
    let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init), ..Default::default() }).await;
    let code = h.key(2);
    h.enter(&code, false).await;
    h.keys("A;  x=[1,2]<Esc>").await;
    h.cmd("w").await;
    let src = h.saved()["cells"][2]["source"].clone();
    // Buffer lines map 1:1 to source lines, so the buffer's final newline is not in the source.
    assert_eq!(src, json!(["plot()\n", "x = [1, 2]"]), "formatted before the commit");
    assert_eq!(h.saved()["cells"][2]["outputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn write_hooks_run_on_modified_cells_when_writing_from_home() {
    // A common config: trim trailing whitespace in whatever buffer is being written.
    let init = r#"
        vim.api.nvim_create_autocmd('BufWritePre', {
          pattern = '*',
          callback = function() vim.cmd([[%s/\s\+$//e]]) end,
        })
    "#;
    let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init.into()), ..Default::default() }).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    h.keys("Goy = 2   <Esc>").await;
    h.leave().await;
    h.cmd("w").await;
    assert_eq!(h.saved()["cells"][1]["source"], json!(["import numpy as np\n", "np.arange(3)\n", "y = 2"]));
}

#[tokio::test]
async fn quitting_a_cell_window_returns_home() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    h.keys("Goy = 2<Esc>").await;
    h.cmd("q").await;
    h.wait("home after :q", |s| matches!(s.focus, Some((_, Focus::Home)))).await;
    assert!(!h.state.lock().unwrap().exited, ":q in a cell leaves the cell, not Neovim");
    assert_eq!(h.source(1), "import numpy as np\nnp.arange(3)\ny = 2");

    // Re-entered, the cell's new window has the cursor where it was left.
    h.enter(&code, true).await;
    h.keys("z").await;
    // Leaving works from insert mode, behind keys already typed.
    h.leave().await;
    assert_eq!(h.lua("return vim.api.nvim_get_mode().mode").await.as_str(), Some("n"));
    assert_eq!(h.source(1), "import numpy as np\nnp.arange(3)\ny = z2");
}

#[tokio::test]
async fn esc_in_normal_mode_leaves_the_cell() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, true).await;
    h.keys("x = 1").await;
    // From insert mode, the first Esc only returns to Normal mode.
    h.keys("<Esc>").await;
    assert!(matches!(h.state.lock().unwrap().focus, Some((_, Focus::Cell(_)))));
    h.cmd("let @/ = 'np' | set hlsearch").await;
    h.keys("<Esc>").await;
    h.wait("home after Esc", |s| matches!(s.focus, Some((_, Focus::Home)))).await;
    assert_eq!(h.lua("return vim.v.hlsearch").await.as_u64(), Some(0), "search highlighting cleared");
    assert_eq!(h.source(1), "x = 1import numpy as np\nnp.arange(3)");
}

#[tokio::test]
async fn unsaved_changes_refuse_quit() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    h.layout_all(None).await;
    // A structural change or output marks the home buffer.
    h.state.lock().unwrap().editor.set_modified();
    h.settle().await;
    let err = h.try_cmd("q").await.unwrap_err();
    assert!(err.contains("E37") || err.contains("E162"), "{err}");
    h.cmd("w").await;
    assert_eq!(h.state.lock().unwrap().written, 1);
    assert!(h.try_cmd("q").await.is_ok() || h.state.lock().unwrap().exited);
}

#[tokio::test]
async fn edits_in_a_hidden_cell_buffer_refuse_quit() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    h.keys("ox<Esc>").await;
    h.leave().await;
    let err = h.try_cmd("q").await.unwrap_err();
    assert!(err.contains("E162") || err.contains("E37"), "{err}");
}

#[tokio::test]
async fn structural_changes_rewrite_or_wipe_buffers() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    h.layout_all(None).await;
    let code = h.key(1);
    let buf = h.state.lock().unwrap().editor.buffer(&code).unwrap();
    {
        let mut st = h.state.lock().unwrap();
        let new = st.nb.new_cell(nbv_core::CellKind::Code, "");
        st.nb.edit(vec![Change::Split { key: code.clone(), line: 1, new }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *st;
        editor.sync(nb);
    }
    h.settle().await;
    let lines = h.lua(&format!("return vim.api.nvim_buf_get_lines({buf}, 0, -1, false)")).await;
    assert_eq!(lines.as_array().unwrap().len(), 1, "the first half's buffer was rewritten");
    assert_eq!(h.source(1), "import numpy as np");

    {
        let mut st = h.state.lock().unwrap();
        st.nb.edit(vec![Change::Hide { key: code.clone() }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *st;
        editor.sync(nb);
    }
    h.settle().await;
    assert_eq!(h.lua(&format!("return vim.api.nvim_buf_is_valid({buf})")).await.as_bool(), Some(false));
    assert!(h.state.lock().unwrap().editor.buffer(&code).is_none());
    // Laid out again after undo, the cell gets a fresh buffer with its text.
    h.state.lock().unwrap().nb.undo();
    h.layout_all(None).await;
    assert!(h.state.lock().unwrap().editor.buffer(&code).is_some());
}

#[tokio::test]
async fn reload_rebuilds_from_disk() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    h.enter(&code, false).await;
    h.keys("ddu").await;
    h.keys("dd").await;
    assert_eq!(h.source(1), "np.arange(3)");
    h.cmd("e!").await;
    h.wait("reloaded", |s| s.nb.cell(&CellKey::new("a1b2c3d4")).unwrap().source().starts_with("import")).await;
    h.wait("cell buffers wiped", |s| s.editor.buffer(&CellKey::new("a1b2c3d4")).is_none()).await;
}

#[tokio::test]
async fn the_home_window_is_transparent_and_cells_are_not() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let rects = h.layout_all(None).await;
    let screen = h.opaque_screen();
    let rows: Vec<&str> = screen.lines().collect();
    for r in &rects {
        let row = rows[r.row as usize];
        let cells: String = row.chars().skip(r.col as usize).take(r.width as usize).collect();
        assert!(!cells.contains('·'), "cell window row {} is opaque\n{screen}", r.row);
    }
    // Row 0 and the column left of the windows belong to the home window.
    assert!(rows[0].chars().all(|c| c == '·'), "{screen}");
    assert!(rows[1].starts_with("····"), "{screen}");
    assert!(rows[1].contains("## Results"), "{screen}");
    assert!(rows[3].contains("import numpy as np"), "{screen}");
}

#[tokio::test]
async fn home_stays_transparent_under_user_decorations() {
    let init = r#"
        vim.o.number = true
        vim.o.cursorline = true
        vim.o.signcolumn = 'yes'
        vim.o.list = true
    "#;
    let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init.into()), ..Default::default() }).await;
    let rects = h.layout_all(None).await;
    let screen = h.opaque_screen();
    let rows: Vec<&str> = screen.lines().collect();
    assert!(rows[0].chars().all(|c| c == '·'), "{screen}");
    // Cell windows take the user's options: line numbers show inside them.
    let first = &rows[rects[1].row as usize];
    assert!(first.contains("1 import numpy"), "{screen}");
}

#[tokio::test]
async fn windows_are_clipped_with_skip() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let code = h.key(1);
    // Only the second line of `import numpy as np / np.arange(3)` is visible.
    let rect = nbv_nvim::EditorRect { key: code, row: 2, col: 4, width: 30, height: 1, skip: Some(1) };
    h.layout(None, &[rect]).await;
    let screen = h.screen();
    let row: String = screen.lines().nth(2).unwrap().to_string();
    assert!(row.contains("np.arange(3)"), "{screen}");
    assert!(!screen.contains("import numpy"), "{screen}");
    assert!(!screen.contains("## Results"), "windows of unlisted cells are closed\n{screen}");
}

#[tokio::test]
async fn windows_wrap_and_start_on_whole_lines() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let md = h.key(0);
    let long = format!("{}{}{}", "a".repeat(20), "b".repeat(20), "c".repeat(10));
    h.state.lock().unwrap().nb.set_source(&md, &format!("short\n{long}\nend"));
    // The window shows the rows of the cell from `skip` down to screen row 6.
    let rect = |skip: usize| nbv_nvim::EditorRect {
        key: md.clone(),
        row: 2,
        col: 4,
        width: 20,
        height: 5 - skip as u16,
        skip: Some(skip),
    };
    h.layout(None, &[rect(0)]).await;
    // `short`, the long line in three rows, `end`.
    h.wait("rows measured", |s| s.editor.rows(&md, 20) == Some(5)).await;
    let rows = || -> Vec<String> {
        h.screen()
            .lines()
            .skip(2)
            .take(5)
            .map(|l| l.chars().skip(4).take(20).collect::<String>().trim().to_string())
            .collect()
    };
    let (a, b, c) = ("a".repeat(20), "b".repeat(20), "c".repeat(10));
    h.layout(None, &[rect(1)]).await;
    assert_eq!(rows(), [a.as_str(), &b, &c, "end", ""]);
    // Rows inside the long line: the window starts at the next line, lower down.
    h.layout(None, &[rect(2)]).await;
    assert_eq!(rows(), ["", "", "end", "", ""]);
    h.layout(None, &[rect(3)]).await;
    assert_eq!(rows(), ["", "end", "", "", ""]);
    h.layout(None, &[rect(4)]).await;
    assert_eq!(rows(), ["end", "", "", "", ""]);
    // A window whose rows all fall inside the line is hidden.
    let tail = nbv_nvim::EditorRect { height: 1, ..rect(2) };
    h.layout(None, &[tail]).await;
    assert_eq!(rows(), ["", "", "", "", ""]);
}
