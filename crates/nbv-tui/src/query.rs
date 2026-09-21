//! Terminal capability queries without a reader thread: write the queries, then read the
//! replies with `poll(2)` and a deadline. A query nobody answers costs a timeout, never a
//! thread left blocked on stdin that would later swallow keystrokes.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Replies {
    /// Cell size in pixels (width, height), from `CSI 16 t`.
    pub cell: Option<(u16, u16)>,
    /// DA1 lists Sixel graphics (attribute 4). Inside tmux, tmux answers for itself.
    pub sixel: bool,
    /// Whether the terminal answered at all.
    pub answered: bool,
}

/// Sends `CSI 16 t` (cell size) and DA1, reading until the DA1 reply or `timeout`. Terminals
/// answer in order, so DA1 arriving means the cell-size reply either came or never will.
/// The terminal must be in raw mode.
pub fn query(timeout: Duration) -> Replies {
    let mut out = std::io::stdout();
    if out.write_all(b"\x1b[16t\x1b[c").and_then(|_| out.flush()).is_err() {
        return Replies::default();
    }
    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    while !has_da1(&buf) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: reading into a local buffer of the stated length; poll said it is readable.
        let r = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if r <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..r as usize]);
    }
    parse(&buf)
}

fn has_da1(buf: &[u8]) -> bool {
    csi_sequences(buf).any(|(params, fin)| fin == b'c' && params.starts_with('?'))
}

/// Yields `(parameters, final byte)` of each CSI sequence.
fn csi_sequences(buf: &[u8]) -> impl Iterator<Item = (String, u8)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 1 < buf.len() {
            if buf[i] == 0x1b && buf[i + 1] == b'[' {
                let start = i + 2;
                let mut j = start;
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j >= buf.len() {
                    return None;
                }
                i = j + 1;
                return Some((String::from_utf8_lossy(&buf[start..j]).into_owned(), buf[j]));
            }
            i += 1;
        }
        None
    })
}

fn parse(buf: &[u8]) -> Replies {
    let mut r = Replies { answered: !buf.is_empty(), ..Default::default() };
    for (params, fin) in csi_sequences(buf) {
        match fin {
            b't' => {
                let p: Vec<u16> = params.split(';').filter_map(|x| x.parse().ok()).collect();
                if let [6, h, w] = p[..]
                    && w > 0
                    && h > 0
                {
                    r.cell = Some((w, h));
                }
            }
            b'c' if params.starts_with('?') => {
                r.sixel = params[1..].split(';').any(|a| a == "4");
            }
            _ => {}
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cell_size_and_sixel() {
        let r = parse(b"\x1b[6;20;10t\x1b[?62;4;22c");
        assert_eq!(r, Replies { cell: Some((10, 20)), sixel: true, answered: true });
        let r = parse(b"\x1b[?1;2c");
        assert_eq!(r, Replies { cell: None, sixel: false, answered: true });
        assert!(!has_da1(b"\x1b[6;20;10t"));
    }
}
