//! The spike-tile grid in the main window: the spikes waiting for the GPU, the tile slots,
//! recent firing times for the rate label, and the drawing.

use crate::config::DisplaySettings;
use crate::spikes::{SLOTS, SpikeCallback, SpikeFrame};
use eframe::egui::{self, Color32, Stroke};
use eframe::egui_wgpu;
use engine::ChannelStats;
use engine::dsp::Spike;
use engine::rhythm::StreamLayout;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use super::{monitor_color32, wheel_rows};

/// What the grid reads from the rest of the app each frame.
pub struct GridInput<'a> {
    pub layout: &'a StreamLayout,
    pub display: &'a DisplaySettings,
    /// Audio-monitored channels (for the tile colours).
    pub monitored: &'a [usize],
    pub stats: &'a ChannelStats,
}

/// What the rest of the app acts on after a frame of the grid.
pub struct GridOut {
    /// The first row shown, to be stored in the display settings.
    pub first_row: usize,
    /// The wheel moved the grid (the config needs saving).
    pub scrolled: bool,
    /// A tile was clicked: monitor that channel (Ctrl or Shift: toggle it in the mix).
    pub clicked: Option<usize>,
}

pub struct SpikeGrid {
    t0: Instant,
    /// Next tile slot per channel.
    slot: Vec<usize>,
    /// Spikes not yet handed to the GPU: (channel, slot, time, spike).
    new: Vec<(usize, usize, f32, Arc<Spike>)>,
    /// Arrival times of the last 2 s of spikes per channel (the rate label).
    times: Vec<VecDeque<f64>>,
    /// Clear the persistence image at the next frame.
    reset: bool,
    /// Wheel movement over the grid not yet turned into rows (`wheel_rows`).
    scroll: f32,
    /// Channel colours to upload at the next frame.
    colors: Option<Arc<Vec<[f32; 4]>>>,
}

impl SpikeGrid {
    pub fn new() -> Self {
        Self {
            t0: Instant::now(),
            slot: Vec::new(),
            new: Vec::new(),
            times: Vec::new(),
            reset: true,
            scroll: 0.0,
            colors: None,
        }
    }

    /// New views (board, layout or scan changed): clear the image and what waits for it.
    pub fn reset(&mut self) {
        self.reset = true;
        self.new.clear();
    }

    pub fn set_colors(&mut self, colors: Arc<Vec<[f32; 4]>>) {
        self.colors = Some(colors);
    }

    /// Takes the spikes that arrived from the engine (`None` without a spike queue) for a
    /// layout of `nel` electrodes.
    pub fn ingest(&mut self, nel: usize, spikes: Option<impl Iterator<Item = Spike>>) {
        if self.slot.len() != nel {
            self.slot = vec![0; nel];
            self.times = vec![VecDeque::new(); nel];
            self.reset = true;
        }
        let now = self.t0.elapsed().as_secs_f64();
        if let Some(rx) = spikes {
            for sp in rx {
                if sp.channel >= nel {
                    continue;
                }
                let slot = self.slot[sp.channel];
                self.slot[sp.channel] = (slot + 1) % SLOTS;
                self.times[sp.channel].push_back(now);
                self.new.push((sp.channel, slot, now as f32, Arc::new(sp)));
            }
            // Drained only while the main window draws: when it's hidden for hours, keep just
            // what can still be shown, the newest spike in each tile slot (nel × SLOTS).
            if self.new.len() > 2 * nel * SLOTS {
                compact_newest_per_slot(&mut self.new);
            }
        }
        for q in &mut self.times {
            while q.front().is_some_and(|&t| now - t > 2.0) {
                q.pop_front();
            }
        }
    }

    /// Draws the grid in `rect`.
    pub fn ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect, inp: &GridInput) -> GridOut {
        let l = inp.layout;
        let nel = l.num_electrodes();
        let d = inp.display;
        let ppp = ui.ctx().pixels_per_point();
        let gc = d.spike_grid_cols.max(1) as usize;
        let cell_w = rect.width() / gc as f32;
        let cell_h = (cell_w * 0.75).max(40.0);
        let rows_fit = ((rect.height() / cell_h).floor() as usize).max(1);
        // Mouse wheel scrolls the grid by rows of tiles.
        let total_rows = nel.div_ceil(gc);
        let max_first = total_rows.saturating_sub(rows_fit);
        let hovered = ui.rect_contains_pointer(rect);
        let scroll = if hovered {
            ui.input(|i| i.smooth_scroll_delta.y)
        } else {
            0.0
        };
        let mut first_row = d.spike_first_row.min(max_first);
        let mut scrolled = false;
        let step = wheel_rows(&mut self.scroll, scroll);
        if step != 0 {
            first_row = (first_row as isize - step).clamp(0, max_first as isize) as usize;
            scrolled = true;
        }
        let first = first_row * gc;
        let ncells = (gc * rows_fit).min(nel.saturating_sub(first));
        let now = self.t0.elapsed().as_secs_f32();
        let frame = SpikeFrame {
            nch: nel,
            reset: std::mem::take(&mut self.reset),
            new: std::mem::take(&mut self.new),
            vp_px: [rect.width() * ppp, rect.height() * ppp],
            cell_px: [cell_w * ppp, cell_h * ppp],
            grid_cols: gc as u32,
            first_ch: first as u32,
            ncells: ncells as u32,
            uv_range: d.spike_uv_range,
            now,
            tau: d.spike_tau_s,
            thickness: ppp.max(1.0),
            colors: self.colors.take().map(|c| {
                Arc::new(
                    c.iter()
                        .take(nel)
                        .copied()
                        .map(|mut x| {
                            x[3] = 0.55;
                            x
                        })
                        .collect(),
                )
            }),
        };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(10, 10, 12));
        ui.painter()
            .add(egui_wgpu::Callback::new_paint_callback(rect, SpikeCallback(frame)));
        let resp = ui.interact(rect, ui.scope_id().with("spike_tiles"), egui::Sense::click());
        let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        let mut clicked_ch = None;
        let mon = inp.monitored;
        let p = ui.painter_at(rect);
        let cs = inp.stats;
        for i in 0..ncells {
            let ch = first + i;
            let o = rect.min + egui::vec2((i % gc) as f32 * cell_w, (i / gc) as f32 * cell_h);
            let cell = egui::Rect::from_min_size(o, egui::vec2(cell_w, cell_h));
            if click.is_some_and(|pos| cell.contains(pos)) {
                clicked_ch = Some(ch);
            }
            let mc = monitor_color32(mon, ch);
            let monitored = mc.is_some();
            let (border, label) = match mc {
                Some(c) => (Stroke::new(1.5, c), c),
                None => (Stroke::new(1.0, Color32::from_gray(35)), Color32::from_gray(160)),
            };
            p.rect_stroke(cell.shrink(1.0), 2.0, border, egui::StrokeKind::Inside);
            let rate = self.times.get(ch).map(|q| q.len() as f32 / 2.0).unwrap_or(0.0);
            p.with_clip_rect(cell.shrink(1.0)).text(
                o + egui::vec2(5.0, 3.0),
                egui::Align2::LEFT_TOP,
                format!(
                    "{}{}  {:.1} Hz",
                    l.channels[ch].name,
                    if monitored { " ♪" } else { "" },
                    rate
                ),
                egui::FontId::monospace(10.5),
                label,
            );
            if let Some(&thr) = cs.threshold_uv.get(ch) {
                let k = (cell_h * 0.5 - 4.0) / d.spike_uv_range;
                let y = o.y + cell_h * 0.5 - thr * k;
                if y > o.y && y < o.y + cell_h {
                    p.line_segment(
                        [egui::pos2(o.x + 4.0, y), egui::pos2(o.x + cell_w - 4.0, y)],
                        Stroke::new(1.0, Color32::from_rgba_unmultiplied(220, 80, 80, 90)),
                    );
                }
                if let Some(s) = cs.sigma_uv.get(ch) {
                    p.text(
                        o + egui::vec2(cell_w - 5.0, cell_h - 3.0),
                        egui::Align2::RIGHT_BOTTOM,
                        format!("σ {s:.1}"),
                        egui::FontId::monospace(10.0),
                        Color32::from_gray(110),
                    );
                }
            }
        }
        GridOut {
            first_row,
            scrolled,
            clicked: clicked_ch,
        }
    }
}

/// Keeps the last entry for each (channel, slot), in order: later spikes overwrite earlier
/// ones in a slot, so the dropped ones would never have been seen.
fn compact_newest_per_slot<T>(v: &mut Vec<(usize, usize, f32, T)>) {
    let mut seen = std::collections::HashSet::new();
    let mut keep: Vec<bool> = v.iter().rev().map(|e| seen.insert((e.0, e.1))).collect();
    keep.reverse();
    let mut k = keep.into_iter();
    v.retain(|_| k.next().unwrap());
}

#[cfg(test)]
mod tests {
    use super::compact_newest_per_slot;

    #[test]
    fn hidden_spike_backlog_keeps_the_newest_per_slot() {
        // Channel 0 slots 0,1,0,1,0 and channel 1 slot 0, in time order (value = time).
        let mut v = vec![
            (0, 0, 1.0, ()),
            (0, 1, 2.0, ()),
            (1, 0, 3.0, ()),
            (0, 0, 4.0, ()),
            (0, 1, 5.0, ()),
            (0, 0, 6.0, ()),
        ];
        compact_newest_per_slot(&mut v);
        let t: Vec<f32> = v.iter().map(|e| e.2).collect();
        assert_eq!(t, vec![3.0, 5.0, 6.0]);
    }
}
