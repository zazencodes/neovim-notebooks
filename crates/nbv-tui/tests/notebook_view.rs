//! The notebook view against a real Neovim (§11): at every scroll offset, each visible cell's
//! window shows exactly the rows the layout gives it, and every other row of the notebook
//! area is transparent, so what nbv draws there (borders, outputs) shows.

use nbv_core::CellKind;
use nbv_nvim::harness::{Harness, Options};
use nbv_tui::layout::{Geometry, Layout};

#[tokio::test]
async fn windows_follow_the_layout_while_scrolling() {
    let h = Harness::open("v4.5-outputs.ipynb", Options { width: 60, height: 14, ..Default::default() }).await;
    let g = Geometry { viewport: h.state.lock().unwrap().viewport.unwrap() };
    let (layout, sources) = {
        let st = h.state.lock().unwrap();
        let cells: Vec<_> = st
            .nb
            .order()
            .iter()
            .map(|k| {
                let c = st.nb.cell(k).unwrap();
                let outputs = if c.kind() == CellKind::Code { 4 } else { 0 };
                (k.clone(), c.kind(), c.source().split('\n').count(), outputs)
            })
            .collect();
        let sources: Vec<Vec<String>> = st
            .nb
            .order()
            .iter()
            .map(|k| st.nb.cell(k).unwrap().source().split('\n').map(String::from).collect())
            .collect();
        (Layout::new(cells), sources)
    };
    let area = g.area_height();
    for scroll in 0..=layout.max_scroll(area) {
        let rects = layout.editors(&g, scroll, None);
        h.layout(None, &rects).await;
        let screen = h.opaque_screen();
        let rows: Vec<&str> = screen.lines().collect();
        let mut covered = vec![false; rows.len()];
        for r in &rects {
            let i = layout.index_of(&r.key).unwrap();
            for dy in 0..r.height {
                let row = (r.row + dy) as usize;
                covered[row] = true;
                let shown: String = rows[row].chars().skip(r.col as usize).take(r.width as usize).collect();
                let line = &sources[i][r.topline - 1 + dy as usize];
                assert!(
                    shown.starts_with(line.as_str()),
                    "scroll {scroll}: row {row} shows {shown:?}, not {line:?}\n{screen}"
                );
            }
        }
        for row in g.area_top() as usize..g.area_top() as usize + area {
            if !covered[row] {
                assert!(rows[row].chars().all(|c| c == '·'), "scroll {scroll}: row {row} is not transparent\n{screen}");
            }
        }
    }
}
