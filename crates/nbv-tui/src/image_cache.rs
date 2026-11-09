//! Byte-bounded terminal encodings and explicit Kitty resource ownership.
use std::collections::{HashMap, hash_map::RandomState};
use std::hash::BuildHasher;
use std::io::Write;

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::{
    FontSize, Resize,
    picker::{Picker, ProtocolType},
    protocol::kitty::Kitty,
    sliced::SlicedProtocol,
};

pub type ImageId = (u64, u16, u16);
pub const PROTOCOL_BYTES: usize = 64 * 1024 * 1024;

pub struct Protocols {
    entries: HashMap<ImageId, SlicedProtocol>,
    used: HashMap<ImageId, u64>,
    kitty_ids: HashMap<ImageId, u32>,
    pending_delete: Vec<u32>,
    next_id: u32,
    clock: u64,
}

impl Default for Protocols {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            used: HashMap::new(),
            kitty_ids: HashMap::new(),
            pending_delete: vec![],
            next_id: RandomState::new().hash_one(std::process::id()) as u32,
            clock: 0,
        }
    }
}

impl Protocols {
    pub fn entries(&self) -> &HashMap<ImageId, SlicedProtocol> {
        &self.entries
    }
    pub fn begin_frame(&mut self) {
        self.clock += 1;
    }
    pub fn touch(&mut self, id: &ImageId) -> bool {
        if let Some(used) = self.used.get_mut(id) {
            *used = self.clock;
            true
        } else {
            false
        }
    }
    pub fn insert(&mut self, id: ImageId, image: &DynamicImage, picker: &Picker) -> Result<(), String> {
        let size = Size::new(id.1, id.2);
        let protocol = if picker.protocol_type() == ProtocolType::Kitty {
            // The upstream Picker hides Kitty's allocated id and its protocol has no Drop
            // cleanup. Own that id explicitly so eviction can delete exactly our resource.
            self.next_id = self.next_id.wrapping_add(1).max(1);
            while self.kitty_ids.values().chain(self.pending_delete.iter()).any(|id| *id == self.next_id) {
                self.next_id = self.next_id.wrapping_add(1).max(1);
            }
            let image = Resize::Fit(None).resize(image, picker.font_size(), size, None);
            let kitty =
                Kitty::new(image, size, self.next_id, picker.tmux_detected(), true).map_err(|e| e.to_string())?;
            self.kitty_ids.insert(id, self.next_id);
            SlicedProtocol::Kitty(kitty)
        } else {
            SlicedProtocol::new(picker, image.clone(), Some(size)).map_err(|e| e.to_string())?
        };
        self.entries.insert(id, protocol);
        self.used.insert(id, self.clock);
        Ok(())
    }
    fn remove(&mut self, id: ImageId) {
        self.entries.remove(&id);
        self.used.remove(&id);
        if let Some(kitty) = self.kitty_ids.remove(&id) {
            self.pending_delete.push(kitty);
        }
    }
    pub fn remove_hash(&mut self, hash: u64) {
        let ids: Vec<_> = self.entries.keys().filter(|id| id.0 == hash).copied().collect();
        for id in ids {
            self.remove(id);
        }
    }
    pub fn clear(&mut self) {
        let ids: Vec<_> = self.entries.keys().copied().collect();
        for id in ids {
            self.remove(id);
        }
    }
    pub fn trim(&mut self, font: FontSize, budget: usize) {
        let mut priority: Vec<_> = self.used.iter().map(|(id, used)| (*id, *used)).collect();
        priority.sort_unstable_by_key(|(id, used)| (std::cmp::Reverse(*used), *id));
        // Visible protocols are pinned for this frame, including oversized plots. Their
        // count/geometry is bounded by the viewport, not the offscreen cache budget.
        let mut bytes = priority
            .iter()
            .filter(|(_, used)| *used == self.clock)
            .fold(0usize, |bytes, (id, _)| bytes.saturating_add(protocol_bytes(*id, font)));
        for (id, used) in priority {
            if used == self.clock {
                continue;
            }
            let charge = protocol_bytes(id, font);
            if bytes.saturating_add(charge) <= budget {
                bytes += charge;
            } else {
                self.remove(id);
            }
        }
    }
    /// Flush before the next frame, after obsolete placements have been invalidated. Uppercase
    /// I frees image data as well as placements; q=2 suppresses unsolicited terminal replies.
    /// https://sw.kovidgoyal.net/kitty/graphics-protocol/#deleting-images
    pub fn flush_deletions(&mut self, out: &mut impl Write, tmux: bool) -> std::io::Result<()> {
        for id in &self.pending_delete {
            let command = format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
            if tmux {
                write!(out, "\x1bPtmux;{}\x1b\\", command.replace('\x1b', "\x1b\x1b"))?;
            } else {
                out.write_all(command.as_bytes())?;
            }
        }
        if !self.pending_delete.is_empty() {
            out.flush()?;
        }
        self.pending_delete.clear();
        Ok(())
    }
}

/// Conservative byte charge: upstream protocol representations are opaque. Covers encoded
/// pixel/transmission strings, sliced rows, halfblock buffers and their allocation overhead.
pub fn protocol_bytes(id: ImageId, font: FontSize) -> usize {
    let cells = usize::from(id.1) * usize::from(id.2);
    cells
        .saturating_mul(usize::from(font.width.max(1)))
        .saturating_mul(usize::from(font.height.max(1)))
        .saturating_mul(128)
        .saturating_add(cells.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
    use ratatui_image::sliced::{SignedPosition, SlicedImage};

    fn render(protocols: &Protocols, id: ImageId) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, id.1, id.2));
        SlicedImage::new(&protocols.entries[&id], SignedPosition::from((0, 0))).render(buf.area, &mut buf);
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    #[ignore = "release performance scenario: cargo test -p nbv-tui --release repeated_plot_protocol_scenario -- --ignored --nocapture"]
    fn repeated_plot_protocol_scenario() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut cache = Protocols::default();
        let mut peak = 0;
        let mut deletes = 0;
        let start = std::time::Instant::now();
        for hash in 0..200 {
            if hash > 0 {
                cache.remove_hash(hash - 1);
            }
            let mut deletion = vec![];
            cache.flush_deletions(&mut deletion, false).unwrap();
            deletes += String::from_utf8(deletion).unwrap().matches("a=d,d=I").count();
            cache.begin_frame();
            let image =
                DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(800, 800, image::Rgba([hash as u8, 0, 0, 255])));
            let id = (hash, 24, 24);
            cache.insert(id, &image, &picker).unwrap();
            assert!(render(&cache, id).contains("a=T"));
            cache.trim(picker.font_size(), PROTOCOL_BYTES);
            let bytes: usize = cache.entries.keys().map(|id| protocol_bytes(*id, picker.font_size())).sum();
            peak = peak.max(bytes);
            assert!(bytes <= PROTOCOL_BYTES);
            assert_eq!(cache.kitty_ids.len(), 1);
        }
        cache.clear();
        let mut deletion = vec![];
        cache.flush_deletions(&mut deletion, false).unwrap();
        deletes += String::from_utf8(deletion).unwrap().matches("a=d,d=I").count();
        assert_eq!(deletes, 200);
        eprintln!(
            "200 replaced 800x800 Kitty plots: {:?}; peak retained protocol charge={peak} bytes; 200 owned resources uploaded and explicitly deleted",
            start.elapsed()
        );
    }

    #[test]
    fn offscreen_live_plots_keep_protocols_and_only_lru_entries_are_evicted() {
        let picker = Picker::halfblocks();
        let mut cache = Protocols::default();
        let image = DynamicImage::new_rgb8(4, 4);
        let budget = protocol_bytes((0, 2, 2), picker.font_size()) * 2;
        for hash in 0..2 {
            cache.begin_frame();
            cache.insert((hash, 2, 2), &image, &picker).unwrap();
        }
        cache.begin_frame(); // A frame containing no images must preserve both live plots.
        cache.trim(picker.font_size(), budget);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.touch(&(0, 2, 2)));
        cache.insert((2, 2, 2), &image, &picker).unwrap();
        cache.trim(picker.font_size(), budget);
        assert!(cache.entries.contains_key(&(0, 2, 2)));
        assert!(!cache.entries.contains_key(&(1, 2, 2)));
        cache.remove_hash(0);
        assert!(!cache.entries.contains_key(&(0, 2, 2)));
    }

    #[test]
    fn oversized_visible_kitty_plot_keeps_its_owned_id_and_upload_until_scrolled_away() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut cache = Protocols::default();
        let id = (7, 80, 24);
        cache.begin_frame();
        cache.insert(id, &DynamicImage::new_rgb8(800, 480), &picker).unwrap();
        let owned = cache.kitty_ids[&id];
        assert!(render(&cache, id).contains("a=T"));
        for _ in 0..10 {
            cache.begin_frame();
            assert!(cache.touch(&id));
            cache.trim(picker.font_size(), 1); // Force an oversized charge without changing font geometry.
            assert_eq!(cache.kitty_ids[&id], owned);
            assert!(!render(&cache, id).contains("a=T"));
            assert!(cache.pending_delete.is_empty());
        }
        cache.begin_frame(); // No touch: the plot has left the viewport.
        cache.trim(picker.font_size(), 1);
        assert!(cache.entries.is_empty());
        let mut deleted = vec![];
        cache.flush_deletions(&mut deleted, false).unwrap();
        assert_eq!(String::from_utf8(deleted).unwrap(), format!("\x1b_Ga=d,d=I,i={owned},q=2\x1b\\"));
    }

    #[test]
    fn kitty_reuses_uploads_and_deletes_owned_ids_on_eviction_and_clear() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut cache = Protocols::default();
        let image = DynamicImage::new_rgb8(4, 4);
        let id = (1, 2, 2);
        cache.insert(id, &image, &picker).unwrap();
        let kitty_id = cache.kitty_ids[&id];
        assert!(render(&cache, id).contains("a=T"));
        cache.begin_frame();
        cache.trim(picker.font_size(), PROTOCOL_BYTES);
        assert!(cache.touch(&id));
        assert!(!render(&cache, id).contains("a=T"), "focus/scroll return must reuse the transmitted image");
        cache.begin_frame(); // The image is now offscreen, so the budget may evict it.
        cache.trim(picker.font_size(), 0);
        let mut deleted = vec![];
        cache.flush_deletions(&mut deleted, false).unwrap();
        assert_eq!(String::from_utf8(deleted).unwrap(), format!("\x1b_Ga=d,d=I,i={kitty_id},q=2\x1b\\"));
        cache.insert(id, &image, &picker).unwrap();
        assert!(render(&cache, id).contains("a=T"));
        cache.clear();
        let mut deleted = vec![];
        cache.flush_deletions(&mut deleted, true).unwrap();
        let deleted = String::from_utf8(deleted).unwrap();
        assert!(deleted.starts_with("\x1bPtmux;\x1b\x1b_Ga=d,d=I,"));
        assert!(deleted.ends_with("\x1b\x1b\\\x1b\\"));
        assert!(cache.pending_delete.is_empty());
    }
}
