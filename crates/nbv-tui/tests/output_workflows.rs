//! Full output inspection uses an ordinary Neovim scratch buffer.
use nbv_core::{Cell, CellKey, CellKind};
use nbv_nvim::harness::{Harness, Options};
use nbv_tui::{
    inspect,
    layout::Layout,
    outputs::{self, Images},
};
use ratatui_image::FontSize;
use serde_json::json;

fn long_cell() -> Cell {
    Cell::from_raw(json!({"cell_type":"code", "metadata":{}, "source":"", "execution_count":null,
        "outputs":[{"output_type":"stream", "name":"stdout", "text":(0..150).map(|i| format!("line{i:03}\n")).collect::<String>()}]
    }).as_object().unwrap().clone())
}

#[test]
fn collapse_changes_layout_without_changing_notebook_outputs() {
    let cell = long_cell();
    let original = cell.clone();
    let mut view = outputs::build(&cell, 80, FontSize::new(8, 16), &mut Images::default());
    let full = Layout::new([(CellKey::new("one"), CellKind::Code, 1, view.height)]);
    view.collapse();
    let collapsed = Layout::new([(CellKey::new("one"), CellKind::Code, 1, view.height)]);
    assert_eq!(collapsed.total, 5);
    assert_eq!(full.total - collapsed.total, outputs::MAX_TEXT_ROWS - 1);
    assert_eq!(cell, original);
    let expanded = outputs::build(&cell, 80, FontSize::new(8, 16), &mut Images::default());
    assert_eq!(expanded.height, outputs::MAX_TEXT_ROWS);
    let mut empty = outputs::OutputView::default();
    empty.collapse();
    assert_eq!(empty.height, 0);
}

#[tokio::test]
async fn output_scratch_supports_full_search_visual_yank_and_close() {
    let h = Harness::open("v4.5-outputs.ipynb", Options::default()).await;
    let lines = serde_json::to_string(&inspect::lines(&long_cell(), None).unwrap()).unwrap();
    let key = h.key(1);
    h.lua(&format!("require('nbv').output_view(vim.json.decode([==[{lines}]==]), '{}')", key.as_str())).await;
    h.settle().await;
    assert_eq!(h.lua("return vim.bo.buftype").await.as_str(), Some("nofile"));
    assert_eq!(h.lua("return vim.bo.modifiable").await.as_bool(), Some(false));
    assert!(h.lua("return vim.api.nvim_buf_line_count(0)").await.as_u64().unwrap() > 150);
    h.keys("gg/line149<CR>yy").await;
    assert_eq!(h.lua("return vim.fn.getreg('\"')").await.as_str(), Some("line149\n"));
    h.keys("gg/line000<CR>vllllly").await;
    assert_eq!(h.lua("return vim.fn.getreg('\"')").await.as_str(), Some("line00"));
    h.cmd("NvbOpenOutput 1 text/html").await;
    h.wait("output command target", |s| s.commands.iter().any(|(a, _)| a == "open_output")).await;
    {
        let st = h.state.lock().unwrap();
        let (_, args) = st.commands.iter().find(|(a, _)| a == "open_output").unwrap();
        assert_eq!(nbv_nvim::editor::field(args, "key").and_then(|v| v.as_str()), Some(key.as_str()));
    }
    assert!(h.try_cmd("NvbOpenOutput html").await.is_err());
    h.lua(&format!("require('nbv').output_view({{'replacement'}}, '{}')", key.as_str())).await;
    h.settle().await;
    assert_eq!(h.lua("local n = 0; for _, w in ipairs(vim.api.nvim_list_wins()) do if vim.w[w].nbv_output_key then n = n + 1 end end; return n").await.as_u64(), Some(1));
    assert_eq!(h.lua("return vim.api.nvim_get_current_line()").await.as_str(), Some("replacement"));
    h.keys("q").await;
    h.wait("return home", |s| matches!(s.focus, Some((_, nbv_nvim::editor::Focus::Home)))).await;
}
