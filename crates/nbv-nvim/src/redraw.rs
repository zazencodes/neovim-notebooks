//! The typed redraw event stream (§10.1). Only the `ext_linegrid` + `ext_hlstate` subset nbv
//! consumes is decoded; everything else is ignored.

use rmpv::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HlAttr {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub sp: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    /// Semantic highlight names this attribute came from (`ext_hlstate` info).
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeInfo {
    pub name: String,
    pub shape: CursorShape,
    pub percentage: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineCell {
    pub text: String,
    pub hl: Option<u32>,
    pub repeat: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RedrawEvent {
    GridResize { grid: u64, width: usize, height: usize },
    GridLine { grid: u64, row: usize, col: usize, cells: Vec<LineCell> },
    GridScroll { grid: u64, top: usize, bot: usize, left: usize, right: usize, rows: i64 },
    GridClear { grid: u64 },
    GridCursorGoto { grid: u64, row: usize, col: usize },
    HlAttrDefine { id: u32, attr: HlAttr },
    DefaultColors { fg: Option<u32>, bg: Option<u32>, sp: Option<u32> },
    ModeInfoSet { modes: Vec<ModeInfo> },
    ModeChange { mode: String, index: usize },
    BusyStart,
    BusyStop,
    SetTitle(String),
    Flush,
}

fn u(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)).unwrap_or(0)
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn color(v: &Value) -> Option<u32> {
    v.as_i64().filter(|c| *c >= 0).map(|c| c as u32)
}

fn map_get<'a>(m: &'a Value, key: &str) -> Option<&'a Value> {
    m.as_map()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

fn parse_attr(rgb: &Value, info: Option<&Value>) -> HlAttr {
    let flag = |k| map_get(rgb, k).and_then(Value::as_bool).unwrap_or(false);
    let mut names = vec![];
    for item in info.and_then(Value::as_array).into_iter().flatten() {
        if let Some(n) = map_get(item, "hi_name").and_then(Value::as_str) {
            names.push(n.to_string());
        }
    }
    HlAttr {
        fg: map_get(rgb, "foreground").and_then(color),
        bg: map_get(rgb, "background").and_then(color),
        sp: map_get(rgb, "special").and_then(color),
        bold: flag("bold"),
        italic: flag("italic"),
        underline: flag("underline") || flag("underdouble") || flag("underdotted") || flag("underdashed"),
        undercurl: flag("undercurl"),
        strikethrough: flag("strikethrough"),
        reverse: flag("reverse"),
        names,
    }
}

/// Decodes one `redraw` notification's arguments: a list of `[name, args...]` batches.
pub fn parse(args: &[Value]) -> Vec<RedrawEvent> {
    let mut out = vec![];
    for batch in args {
        let Some(batch) = batch.as_array() else { continue };
        let Some(name) = batch.first().and_then(Value::as_str) else { continue };
        for call in &batch[1..] {
            let a = call.as_array().map(Vec::as_slice).unwrap_or(&[]);
            let arg = |n: usize| a.get(n).unwrap_or(&Value::Nil);
            let ev = match name {
                "grid_resize" => RedrawEvent::GridResize {
                    grid: u(arg(0)),
                    width: u(arg(1)) as usize,
                    height: u(arg(2)) as usize,
                },
                "grid_line" => {
                    let cells = arg(3)
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| {
                            let c = c.as_array().map(Vec::as_slice).unwrap_or(&[]);
                            LineCell {
                                text: c.first().and_then(Value::as_str).unwrap_or(" ").to_string(),
                                hl: c.get(1).map(|v| u(v) as u32),
                                repeat: c.get(2).map_or(1, |v| u(v) as usize),
                            }
                        })
                        .collect();
                    RedrawEvent::GridLine { grid: u(arg(0)), row: u(arg(1)) as usize, col: u(arg(2)) as usize, cells }
                }
                "grid_scroll" => RedrawEvent::GridScroll {
                    grid: u(arg(0)),
                    top: u(arg(1)) as usize,
                    bot: u(arg(2)) as usize,
                    left: u(arg(3)) as usize,
                    right: u(arg(4)) as usize,
                    rows: i(arg(5)),
                },
                "grid_clear" => RedrawEvent::GridClear { grid: u(arg(0)) },
                "grid_cursor_goto" => RedrawEvent::GridCursorGoto {
                    grid: u(arg(0)),
                    row: u(arg(1)) as usize,
                    col: u(arg(2)) as usize,
                },
                "hl_attr_define" => RedrawEvent::HlAttrDefine { id: u(arg(0)) as u32, attr: parse_attr(arg(1), a.get(3)) },
                "default_colors_set" => {
                    RedrawEvent::DefaultColors { fg: color(arg(0)), bg: color(arg(1)), sp: color(arg(2)) }
                }
                "mode_info_set" => {
                    let modes = arg(1)
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|m| ModeInfo {
                            name: map_get(m, "name").and_then(Value::as_str).unwrap_or("").to_string(),
                            shape: match map_get(m, "cursor_shape").and_then(Value::as_str) {
                                Some("horizontal") => CursorShape::Horizontal,
                                Some("vertical") => CursorShape::Vertical,
                                _ => CursorShape::Block,
                            },
                            percentage: map_get(m, "cell_percentage").map_or(100, |v| u(v) as u8),
                        })
                        .collect();
                    RedrawEvent::ModeInfoSet { modes }
                }
                "mode_change" => RedrawEvent::ModeChange {
                    mode: arg(0).as_str().unwrap_or("").to_string(),
                    index: u(arg(1)) as usize,
                },
                "busy_start" => RedrawEvent::BusyStart,
                "busy_stop" => RedrawEvent::BusyStop,
                "set_title" => RedrawEvent::SetTitle(arg(0).as_str().unwrap_or("").to_string()),
                "flush" => RedrawEvent::Flush,
                _ => continue,
            };
            out.push(ev);
        }
    }
    out
}
