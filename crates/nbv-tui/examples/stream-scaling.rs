//! Run through scripts/bench-stream.sh to compare this tree with a baseline snapshot.
use std::{hint::black_box, path::Path, time::Instant};

use jupyter_protocol::{JupyterMessage, StreamContent};
use nbv_core::{CellKey, Notebook, exec::Executor};
use nbv_tui::outputs::{Images, TextCache, build_cached};
use ratatui_image::FontSize;

fn main() {
    println!("chunks,bytes,append_ms,append_and_render_ms");
    let chunk = "\x1b[32mstream 世界\x1b[0m: 0123456789 abcdefghijklmnopqrstuvwxyz\n";
    for count in [1_000, 4_000, 16_000] {
        let mut timings = vec![];
        for render in [false, true] {
            let raw = br#"{"cells":[{"cell_type":"code","execution_count":null,"id":"a","metadata":{},"outputs":[],"source":""}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;
            let mut nb = Notebook::from_bytes(Path::new("bench.ipynb"), raw, Default::default()).unwrap();
            let key = CellKey::new("a");
            let mut exec = Executor::default();
            let req = exec.request(&mut nb, &key).unwrap();
            let msg = JupyterMessage::new(StreamContent::stdout(chunk), Some(&req));
            let mut images = Images::default();
            let mut cache = TextCache::default();
            let start = Instant::now();
            for _ in 0..count {
                black_box(exec.handle(&mut nb, &msg));
                if render {
                    black_box(build_cached(nb.cell(&key).unwrap(), 80, FontSize::new(10, 20), &mut images, &mut cache));
                }
            }
            timings.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(nb.cell(&key).unwrap().outputs()[0]["text"].as_array().unwrap().len(), count);
        }
        println!("{count},{},{:.3},{:.3}", count * chunk.len(), timings[0], timings[1]);
    }
}
