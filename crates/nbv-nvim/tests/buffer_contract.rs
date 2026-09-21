//! Spike 2: the buffer contract (§6, §7, §9) through a real Neovim.

mod harness;

use harness::{Harness, Options, repo_root};
use nbv_nvim::NvimClient;
use serde_json::Value;

fn ids(saved: &Value) -> Vec<Option<String>> {
    saved["cells"].as_array().unwrap().iter().map(|c| c["id"].as_str().map(str::to_string)).collect()
}

#[tokio::test]
async fn opens_as_an_ordinary_python_buffer() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    assert_eq!(h.buffer_lines().await, h.mirror());
    let info = h
        .lua("return { vim.bo.filetype, vim.bo.buftype, vim.bo.swapfile, vim.bo.modified, vim.api.nvim_buf_get_name(0) }")
        .await;
    let a = info.as_array().unwrap();
    assert_eq!(a[0].as_str(), Some("python"));
    assert_eq!(a[1].as_str(), Some(""));
    assert_eq!(a[2].as_bool(), Some(false));
    assert_eq!(a[3].as_bool(), Some(false));
    assert!(a[4].as_str().unwrap().ends_with("v4.5-outputs.ipynb.py"));
    // The first load is not an undoable change.
    h.keys("u").await;
    assert_eq!(h.buffer_lines().await, h.mirror());
    assert!(h.screen().contains("# %% [markdown] id=\"e5f6a7b8\""), "{}", h.screen());
}

#[tokio::test]
async fn write_commits_to_the_notebook_and_never_the_projection() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    h.keys("gg]cAX<Esc>").await; // append to the first body line of the second cell
    assert!(h.lua("return vim.bo.modified").await.as_bool().unwrap());
    h.cmd("w").await;
    assert!(!h.lua("return vim.bo.modified").await.as_bool().unwrap());
    let saved = h.saved();
    assert_eq!(saved["cells"][1]["source"], serde_json::json!(["import numpy as npX\n", "np.arange(3)"]));
    assert_eq!(saved["cells"][1]["outputs"][0]["data"]["text/plain"], serde_json::json!(["42"]));
    let names: Vec<_> = std::fs::read_dir(h.dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 1, "only the notebook exists: {names:?}");
}

#[tokio::test]
async fn legacy_notebook_saves_without_ids_until_edited() {
    let h = Harness::open("legacy-4.4-outputs.ipynb", Options::default()).await;
    h.cmd("w").await;
    let saved = h.saved();
    assert_eq!(saved["nbformat_minor"], 4);
    assert!(ids(&saved).iter().all(Option::is_none));

    h.keys("Go# more<Esc>").await;
    h.cmd("w").await;
    let saved = h.saved();
    assert_eq!(saved["nbformat_minor"], 5);
    let keys: Vec<String> = h.state.lock().unwrap().nb.order().iter().map(|k| k.to_string()).collect();
    assert_eq!(ids(&saved), keys.into_iter().map(Some).collect::<Vec<_>>());
}

#[tokio::test]
async fn yank_paste_makes_a_fresh_key_and_delete_undo_restores_outputs() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    // Cell 2 (`a1b2c3d4`) is lines 3-5. Yank it and paste at the end.
    h.cmd("3,5yank").await;
    h.cmd("$put").await;
    h.assert_in_sync().await;
    let (order, lines) = {
        let st = h.state.lock().unwrap();
        (st.nb.order().to_vec(), st.nb.mirror().to_vec())
    };
    assert_eq!(order.len(), 5);
    assert_eq!(order[1].as_str(), "a1b2c3d4");
    assert_ne!(order[4].as_str(), "a1b2c3d4");
    assert!(lines.iter().any(|l| l == &format!("# %% id=\"{}\"", order[4])), "copy normalised");

    // `u` reverts the paste and its normalisation together (undojoin).
    h.keys("u").await;
    h.assert_in_sync().await;
    assert_eq!(h.state.lock().unwrap().nb.order().len(), 4);

    // Delete the cell with outputs, then undo.
    h.keys("3GVjjd").await;
    assert!(h.state.lock().unwrap().nb.is_tombstoned(&nbv_core::CellKey::new("a1b2c3d4")));
    h.keys("u").await;
    h.cmd("w").await;
    let saved = h.saved();
    assert_eq!(saved["cells"][1]["id"], "a1b2c3d4");
    assert_eq!(saved["cells"][1]["outputs"][0]["data"]["text/plain"], serde_json::json!(["42"]));
}

#[tokio::test]
async fn whole_buffer_filter_undo_and_redo_keep_identity() {
    let ruff = repo_root().join(".venv/bin/ruff");
    if !ruff.exists() {
        eprintln!("skipped: no ruff");
        return;
    }
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let before: Vec<String> = h.state.lock().unwrap().nb.order().iter().map(|k| k.to_string()).collect();
    h.keys("4GA;  x=[1,2]<Esc>").await;
    h.cmd(&format!("%!{} format -", ruff.display())).await;
    h.assert_in_sync().await;
    let lines = h.buffer_lines().await;
    assert!(lines.iter().any(|l| l.contains("x = [1, 2]")), "ruff formatted: {lines:?}");
    let after: Vec<String> = h.state.lock().unwrap().nb.order().iter().map(|k| k.to_string()).collect();
    assert_eq!(before, after);
    h.keys("u").await;
    h.assert_in_sync().await;
    h.keys("<C-r>").await;
    h.assert_in_sync().await;
    h.cmd("w").await;
    assert_eq!(h.saved()["cells"][2]["outputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn format_on_save_edits_are_reconciled_before_commit() {
    let init = r#"
        vim.api.nvim_create_autocmd('BufWritePre', {
          pattern = '*.py',
          callback = function(a)
            local lines = vim.api.nvim_buf_get_lines(a.buf, 0, -1, false)
            for i, l in ipairs(lines) do
              lines[i] = l:gsub('np%.arange', 'np.linspace')
            end
            vim.api.nvim_buf_set_lines(a.buf, 0, -1, false, lines)
          end,
        })
    "#;
    let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init.into()), ..Default::default() }).await;
    h.cmd("w").await;
    let saved = h.saved();
    assert_eq!(saved["cells"][1]["source"][1], "np.linspace(3)");
    assert_eq!(saved["cells"][1]["id"], "a1b2c3d4", "identity survives the whole-buffer rewrite");
    assert!(!h.lua("return vim.bo.modified").await.as_bool().unwrap());
}

#[tokio::test]
async fn reload_reads_the_notebook_from_disk() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let original = h.buffer_lines().await;
    h.keys("ggdG").await;
    assert!(h.state.lock().unwrap().nb.order().is_empty());
    h.cmd("e!").await;
    h.wait("reattach", |s| s.nb.order().len() == 4).await;
    assert_eq!(h.buffer_lines().await, original);
    h.assert_in_sync().await;
    assert!(!h.lua("return vim.bo.modified").await.as_bool().unwrap());
    // The buffer is still a notebook buffer: writes still commit.
    h.keys("GoZ = 1<Esc>").await;
    h.cmd("w").await;
    assert_eq!(h.saved()["cells"][3]["source"], serde_json::json!(["\n", "Z = 1"]));
}

#[tokio::test]
async fn writes_elsewhere_and_partial_writes_are_refused() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let other = h.dir().join("other.py");
    let err = h.try_cmd(&format!("w {}", other.display())).await.unwrap_err();
    assert!(err.contains("only be written to its notebook"), "{err}");
    let err = h.try_cmd(&format!("1,2w {}", other.display())).await.unwrap_err();
    assert!(err.contains("partial writes"), "{err}");
    assert!(!other.exists());
}

#[tokio::test]
async fn changed_on_disk_requires_bang() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let mut v = h.saved();
    v["metadata"]["touched"] = true.into();
    std::fs::write(&h.notebook, serde_json::to_vec(&v).unwrap()).unwrap();
    h.keys("GoZ = 1<Esc>").await;
    let err = h.try_cmd("w").await.unwrap_err();
    assert!(err.contains("changed on disk"), "{err}");
    assert!(h.lua("return vim.bo.modified").await.as_bool().unwrap());
    h.cmd("w!").await;
    assert_eq!(h.saved()["cells"][3]["source"], serde_json::json!(["\n", "Z = 1"]));
}

#[tokio::test]
async fn wq_saves_and_exits() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    h.keys("GoZ = 1<Esc>").await;
    let _ = h.client.input(":wq<CR>").await;
    h.wait("exit", |s| s.exited).await;
    assert_eq!(h.saved()["cells"][3]["source"], serde_json::json!(["\n", "Z = 1"]));
}

#[tokio::test]
async fn typing_a_marker_in_insert_mode_normalises_on_leave() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    h.keys("Go# %%<CR>y = 2").await;
    // Still in insert mode: the marker is untouched, but the cell already exists.
    let lines = h.buffer_lines().await;
    assert!(lines.contains(&"# %%".to_string()));
    let pending = h.state.lock().unwrap().nb.order().to_vec();
    assert_eq!(pending.len(), 5);
    h.keys("<Esc>").await;
    let key = pending[4].to_string();
    let lines = h.buffer_lines().await;
    assert!(lines.contains(&format!("# %% id=\"{key}\"")), "{lines:?}");
    assert_eq!(h.state.lock().unwrap().nb.order()[4].as_str(), key, "key stable across normalisation");
    // One `u` removes the typing and the normalisation together.
    h.keys("u").await;
    h.assert_in_sync().await;
    assert_eq!(h.state.lock().unwrap().nb.order().len(), 4);
}

#[tokio::test]
async fn commands_motions_and_text_objects() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    h.keys("gg]c]c").await;
    let row = h.lua("return vim.api.nvim_win_get_cursor(0)[1]").await;
    assert_eq!(row.as_u64(), Some(7), "first body line of the third cell");
    h.keys("[c").await;
    assert_eq!(h.lua("return vim.api.nvim_win_get_cursor(0)[1]").await.as_u64(), Some(4));
    h.keys("dic").await;
    h.assert_in_sync().await;
    assert_eq!(h.state.lock().unwrap().nb.cell(&nbv_core::CellKey::new("a1b2c3d4")).unwrap().source(), "");
    h.cmd("NbvRun").await;
    h.cmd("NbvCellMove down").await;
    let commands: Vec<String> = h.state.lock().unwrap().commands.iter().map(|(a, _)| a.clone()).collect();
    assert_eq!(commands, ["run", "cell_move"]);
}

#[tokio::test]
async fn user_lsp_attaches_through_normal_config() {
    let ruff = repo_root().join(".venv/bin/ruff");
    if !ruff.exists() {
        return;
    }
    let init = format!(
        r#"
        vim.lsp.config('ruff', {{ cmd = {{ '{}', 'server' }}, filetypes = {{ 'python' }}, root_markers = {{ '.git' }} }})
        vim.lsp.enable('ruff')
        "#,
        ruff.display()
    );
    let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init), ..Default::default() }).await;
    let mut name = rmpv::Value::Nil;
    for _ in 0..100 {
        name = h.lua("local c = vim.lsp.get_clients({ bufnr = 0 })[1]; return c and c.name").await;
        if name.as_str().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(name.as_str(), Some("ruff"));
}
