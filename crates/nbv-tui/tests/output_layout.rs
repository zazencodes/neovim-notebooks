//! Spike 3: output layout from the render stream (§11). Placeholders of known heights are
//! placed through the companion, and every frame is checked against invariants that do not
//! depend on the row tags themselves:
//!
//! - a run is never longer than its output;
//! - the rows it shows are consecutive rows of the output;
//! - an output anchored above a marker ends directly on that marker's row whenever its last
//!   row is visible, and when its first rows are hidden it is its tail that shows;
//! - an output clipped at the bottom continues past the window's last text row.

use std::collections::HashMap;

use nbv_nvim::NvimClient;
use nbv_nvim::harness::{Harness, Options};
use nbv_tui::compose::{Run, find_runs};
use nbv_tui::decor::{self, Placement};

/// Output heights for the cells of `v4.5-outputs.ipynb`.
fn heights() -> HashMap<&'static str, usize> {
    HashMap::from([("a1b2c3d4", 6), ("c9d0e1f2", 9)])
}

async fn render(h: &Harness) -> Vec<Placement> {
    h.settle().await;
    let (buf, tick, placements) = {
        let st = h.state.lock().unwrap();
        let hs = heights();
        let p = decor::placements(&st.nb, |k| hs.get(k.as_str()).copied().unwrap_or(0));
        (st.editor.buffer().unwrap(), st.editor.tick(), p)
    };
    h.client
        .exec_lua(
            "require('nbv').render(...)",
            vec![buf.into(), tick.into(), decor::to_value(&placements), rmpv::Value::Array(vec![])],
        )
        .await
        .unwrap();
    h.settle().await;
    placements
}

fn runs(h: &Harness, placements: &[Placement]) -> Vec<(Run, Placement)> {
    let st = h.state.lock().unwrap();
    let by_slot: HashMap<u16, &Placement> = placements.iter().map(|p| (p.slot, p)).collect();
    find_runs(&st.editor.grid, |s| by_slot.get(&s).map(|p| p.height))
        .into_iter()
        .map(|r| {
            let p = by_slot[&r.slot].clone();
            (r, p)
        })
        .collect()
}

fn row_text(h: &Harness, row: usize) -> String {
    let st = h.state.lock().unwrap();
    if row >= st.editor.grid.height { String::new() } else { st.editor.grid.row_text(row) }
}

/// Checks every visible output against the layout invariants and returns the runs.
async fn check(h: &Harness, placements: &[Placement], what: &str) -> Vec<(Run, Placement)> {
    let rs = runs(h, placements);
    let screen = h.screen();
    let mirror = h.mirror();
    for (r, p) in &rs {
        let ctx = || format!("{what}: {r:?} {p:?}\n{screen}");
        assert!(r.len as usize <= p.height, "run longer than output: {}", ctx());
        assert!(r.offset + r.len as usize <= p.height, "rows past the output: {}", ctx());
        let anchor = &mirror[p.line];
        let below = row_text(h, (r.top + r.len) as usize);
        // With --clean, a window's last row is its statusline, showing the buffer name.
        let window_ends = below.contains(".ipynb.py");
        if p.above {
            if r.offset + r.len as usize == p.height {
                assert!(
                    below.starts_with(anchor.as_str()) || window_ends,
                    "output not directly above its anchor: {}",
                    ctx()
                );
            } else {
                assert_eq!(r.offset, 0, "a clipped head must show the top rows: {}", ctx());
                assert!(!below.starts_with("# %%"), "bottom-clipped output ends at a marker: {}", ctx());
            }
        }
    }
    rs
}

fn find<'a>(rs: &'a [(Run, Placement)], key: &str) -> Option<&'a Run> {
    rs.iter().find(|(_, p)| p.key.as_str() == key).map(|(r, _)| r)
}

async fn open(width: usize, height: usize) -> (Harness, Vec<Placement>) {
    let h = Harness::open("v4.5-outputs.ipynb", Options { width, height, ..Default::default() }).await;
    let p = render(&h).await;
    (h, p)
}

#[tokio::test]
async fn outputs_sit_directly_above_the_next_marker() {
    let (h, p) = open(80, 30).await;
    let rs = check(&h, &p, "initial").await;
    // Lines 0-4 are the markdown cell and cell a; its 6 output rows follow at rows 5-10.
    let a = find(&rs, "a1b2c3d4").unwrap();
    assert_eq!((a.top, a.len, a.offset), (5, 6, 0));
    let c = find(&rs, "c9d0e1f2").unwrap();
    assert_eq!((c.top, c.len, c.offset), (13, 9, 0));
}

#[tokio::test]
async fn scrolling_line_by_line_clips_consistently() {
    let (h, p) = open(80, 12).await;
    for step in 0..30 {
        check(&h, &p, &format!("scroll down {step}")).await;
        h.keys("<C-e>").await;
    }
    for step in 0..30 {
        check(&h, &p, &format!("scroll up {step}")).await;
        h.keys("<C-y>").await;
    }
    let rs = check(&h, &p, "back at top").await;
    assert_eq!(find(&rs, "a1b2c3d4").unwrap().top, 5);
}

#[tokio::test]
async fn zt_zz_and_cursor_motion() {
    let (h, p) = open(80, 14).await;
    for keys in ["6Gzt", "6Gzz", "6Gzb", "8Gzt", "Gzz", "gg", "G", "gg<C-d>", "<C-u>"] {
        h.keys(keys).await;
        check(&h, &p, keys).await;
    }
}

#[tokio::test]
async fn insertion_above_an_output_moves_it_down() {
    let (h, p) = open(80, 30).await;
    let before = find(&check(&h, &p, "before").await, "c9d0e1f2").unwrap().top;
    h.keys("4Goinserted = 1<CR>another = 2<Esc>").await;
    let p = render(&h).await;
    let after = find(&check(&h, &p, "after insert").await, "c9d0e1f2").unwrap().top;
    assert_eq!(after, before + 2);
    // Wrapping: a line wider than the window takes two rows.
    h.keys(&format!("o{}<Esc>", "x".repeat(100))).await;
    let p = render(&h).await;
    let wrapped = find(&check(&h, &p, "after wrap").await, "c9d0e1f2").unwrap().top;
    assert_eq!(wrapped, after + 2);
}

#[tokio::test]
async fn deleting_and_undoing_an_anchor() {
    let (h, p) = open(80, 30).await;
    let initial: Vec<Run> = check(&h, &p, "initial").await.into_iter().map(|(r, _)| r).collect();
    // Delete cell c9d0e1f2's marker: its body joins cell a, and its output goes with it.
    h.keys("6Gdd").await;
    let p2 = render(&h).await;
    let rs = check(&h, &p2, "after dd").await;
    assert!(find(&rs, "c9d0e1f2").is_none());
    h.keys("u").await;
    let p3 = render(&h).await;
    let restored: Vec<Run> = check(&h, &p3, "after undo").await.into_iter().map(|(r, _)| r).collect();
    assert_eq!(restored, initial);
    h.keys("<C-r>").await;
    let p4 = render(&h).await;
    check(&h, &p4, "after redo").await;
}

#[tokio::test]
async fn folds_do_not_hide_outputs_anchored_below_them() {
    let (h, p) = open(80, 30).await;
    h.cmd("4,5fold").await;
    let rs = check(&h, &p, "folded").await;
    let a = find(&rs, "a1b2c3d4").unwrap();
    assert_eq!(a.top, 4, "the folded body takes one row; the output follows it");
}

#[tokio::test]
async fn resize_and_splits() {
    let (h, _) = open(80, 30).await;
    h.resize(50, 16).await;
    let p = render(&h).await;
    check(&h, &p, "resized").await;
    h.resize(100, 40).await;
    h.cmd("split").await;
    let rs = check(&h, &p, "split").await;
    let copies = rs.iter().filter(|(_, p)| p.key.as_str() == "a1b2c3d4").count();
    assert_eq!(copies, 2, "both windows show the output\n{}", h.screen());
    h.cmd("only").await;
    h.cmd("vsplit").await;
    let rs = check(&h, &p, "vsplit").await;
    let a: Vec<&Run> = rs.iter().filter(|(_, p)| p.key.as_str() == "a1b2c3d4").map(|(r, _)| r).collect();
    assert_eq!(a.len(), 2);
    assert!(a[0].x1 <= a[1].x0 || a[1].x1 <= a[0].x0, "side by side runs stay separate: {a:?}");
}

#[tokio::test]
async fn floats_cover_outputs_without_moving_them() {
    let (h, p) = open(80, 30).await;
    let before = find(&check(&h, &p, "before").await, "c9d0e1f2").unwrap().clone();
    h.lua(
        "local b = vim.api.nvim_create_buf(false, true)
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'float' })
         vim.api.nvim_open_win(b, false, { relative = 'editor', row = 14, col = 0, width = 20, height = 3 })",
    )
    .await;
    h.settle().await;
    let rs = check(&h, &p, "float").await;
    let after = find(&rs, "c9d0e1f2").unwrap();
    assert_eq!((after.top, after.len, after.offset), (before.top, before.len, before.offset));
    assert!(h.screen().lines().nth(14).unwrap().starts_with("float"));
}
