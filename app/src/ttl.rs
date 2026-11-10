//! TTL-input overlay for the sweeping trace display.
//!
//! Folds each sample's 16-bit TTL word into per-pixel-column state using exactly the trace
//! shader's column mapping (`col = pos * cols / window`, reset when the sweep enters a
//! column), so bands line up with the traces. Drawing is a handful of egui rectangles per
//! high run, not per column.

use eframe::egui::{self, Color32};

/// Column state: bit 16 = column holds data; bits 0..16 = OR of the TTL words in the column.
const VALID: u32 = 1 << 16;

/// Distinct, colour-blind-friendly hues for TTL lines 1..8 (repeats for 9..16).
const LINE_COLORS: [[u8; 3]; 8] = [[255, 176, 0], [100, 143, 255], [220, 38, 127], [120, 220, 120], [254, 97, 0], [120, 94, 240], [80, 220, 220], [230, 230, 230]];

pub fn line_color(line: usize) -> Color32 {
    let [r, g, b] = LINE_COLORS[line % LINE_COLORS.len()];
    Color32::from_rgb(r, g, b)
}

/// Where to draw (points), plus `ppp` (physical px per point; one column per physical px)
/// and the sweep cursor column.
pub struct TtlGeom {
    pub plot: egui::Rect,
    pub strip: egui::Rect,
    pub label_x: f32,
    pub ppp: f32,
    pub cursor: u32,
    /// Columns hidden ahead of the cursor; `None` = the live default (as the traces).
    pub gap: Option<u32>,
    /// Review: the data column drawn at column 0, and the image's pan offset (columns) and
    /// zoom relative to the plot (as the traces). Live: (0, 0, 1).
    pub rot: u32,
    pub x_shift: f32,
    pub x_scale: f32,
}

impl TtlGeom {
    pub fn live(plot: egui::Rect, strip: egui::Rect, label_x: f32, ppp: f32, cursor: u32, gap: Option<u32>) -> Self {
        Self { plot, strip, label_x, ppp, cursor, gap, rot: 0, x_shift: 0.0, x_scale: 1.0 }
    }
}

#[derive(Default)]
pub struct TtlOverlay {
    cols: Vec<u32>,
    cur: u32,
    /// Lines seen high since the last reset; only these get a lane and bands.
    seen: u16,
}

impl TtlOverlay {
    pub fn reset(&mut self) {
        self.cols.clear();
        self.cur = u32::MAX;
        self.seen = 0;
    }

    /// An overlay from one TTL word per column (the OR of the words in that column), e.g.
    /// from the min/max pyramid.
    pub fn from_columns(words: &[u16]) -> Self {
        Self { cols: words.iter().map(|&w| VALID | w as u32).collect(), cur: u32::MAX, seen: words.iter().fold(0, |a, &w| a | w) }
    }

    /// Forget column contents (display geometry changed) but keep the lanes.
    pub fn clear_cols(&mut self) {
        self.cols.clear();
        self.cur = u32::MAX;
    }

    /// Bitmask of the lines seen high since the last reset.
    pub fn seen(&self) -> u16 {
        self.seen
    }


    /// Fold `words` (one per new sample, the first at window position `pos0`).
    pub fn ingest(&mut self, words: &[u16], pos0: u32, window: u32, cols: u32) {
        if self.cols.len() != cols as usize {
            self.cols = vec![0; cols as usize];
            self.cur = u32::MAX;
        }
        let (fw, fc) = (window as f32, cols as f32);
        for (i, &w) in words.iter().enumerate() {
            let pos = (pos0 + i as u32) % window;
            // Same f32 arithmetic as traces.wgsl so columns agree at boundaries.
            let col = ((pos as f32 * fc / fw) as u32).min(cols - 1);
            let c = &mut self.cols[col as usize];
            if col != self.cur {
                *c = VALID | w as u32;
                self.cur = col;
            } else {
                *c |= w as u32;
            }
            self.seen |= w;
        }
    }

    /// Runs `[start, end)` of columns where `line` is high, skipping empty columns and the
    /// stale gap just ahead of the cursor (hidden like the traces).
    fn runs(&self, line: usize, cursor: u32, gap: u32, rot: u32) -> Vec<(u32, u32)> {
        let n = self.cols.len() as u32;
        let mut out = Vec::new();
        let mut start = None;
        for col in 0..n {
            let dc = (col + rot) % n;
            let d = (dc + n - cursor) % n;
            let v = self.cols[dc as usize];
            let high = v & VALID != 0 && v & (1 << line) != 0 && !(d >= 1 && d <= gap);
            match (high, start) {
                (true, None) => start = Some(col),
                (false, Some(s)) => {
                    out.push((s, col));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            out.push((s, n));
        }
        out
    }

    /// Draw bands over `g.plot` and one lane per line in `g.strip`.
    pub fn draw(&self, p: &egui::Painter, g: &TtlGeom, lines: &[usize]) {
        let TtlGeom { plot, strip, label_x, ppp, cursor, gap, rot, x_shift, x_scale } = *g;
        if lines.is_empty() || self.cols.is_empty() {
            return;
        }
        let gap = gap.unwrap_or((self.cols.len() as u32 / 80).max(2));
        let lane_h = strip.height() / lines.len() as f32;
        for (k, &line) in lines.iter().enumerate() {
            let c = line_color(line);
            let band = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 22);
            let y0 = strip.min.y + k as f32 * lane_h;
            let lane = egui::Rect::from_min_max(egui::pos2(strip.min.x, y0 + 0.5), egui::pos2(strip.max.x, y0 + lane_h - 0.5));
            p.rect_filled(lane, 0.0, Color32::from_gray(22));
            for (s, e) in self.runs(line, cursor, gap, rot) {
                let x0 = (plot.min.x + (s as f32 * x_scale + x_shift) / ppp).max(plot.min.x);
                let x1 = (plot.min.x + (e as f32 * x_scale + x_shift) / ppp).min(plot.max.x);
                if x1 <= x0 {
                    continue;
                }
                p.rect_filled(egui::Rect::from_min_max(egui::pos2(x0, plot.min.y), egui::pos2(x1.max(x0 + 1.0 / ppp), plot.max.y)), 0.0, band);
                p.rect_filled(egui::Rect::from_min_max(egui::pos2(x0, lane.min.y), egui::pos2(x1.max(x0 + 1.0 / ppp), lane.max.y)), 0.0, c);
            }
            if lane_h >= 7.0 {
                p.text(egui::pos2(label_x, lane.center().y), egui::Align2::LEFT_CENTER, format!("TTL{}", line + 1), egui::FontId::monospace((lane_h * 0.8).clamp(7.0, 11.0)), c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_reset_on_entry_and_runs_skip_gap() {
        let mut t = TtlOverlay::default();
        t.reset();
        // 100 samples over 10 columns: line 0 high for samples 20..50 (columns 2..5).
        let words: Vec<u16> = (0..100).map(|i| (20..50).contains(&i) as u16).collect();
        t.ingest(&words, 0, 100, 10);
        assert_eq!(t.seen(), 1);
        assert_eq!(t.runs(0, 9, 0, 0), vec![(2, 5)]);
        // Cursor at column 1: columns 2..=3 are the gap ahead of it and hidden.
        assert_eq!(t.runs(0, 1, 2, 0), vec![(4, 5)]);
        // Rotated by 2 (review: data column 2 drawn at column 0): the run moves left.
        assert_eq!(t.runs(0, 9, 0, 2), vec![(0, 3)]);
        // Next sweep: line low everywhere; entering each column resets it.
        t.ingest(&[0; 100], 0, 100, 10);
        assert_eq!(t.runs(0, 9, 0, 0), vec![]);
        assert_eq!(t.seen(), 1, "seen persists until reset");
    }
}
