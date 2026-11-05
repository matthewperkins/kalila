//! Trigger bookkeeping for TTL-triggered sweeps (CPU side; the GPU cuts the data).
//!
//! Finds rising edges of one TTL line in the per-sample TTL words that already travel with
//! the trace upload, waits until each trigger's post-window has been uploaded, and hands out
//! sweep slots. Absolute sample indices follow the display's `disp_counter`.

use std::collections::VecDeque;

#[derive(Default)]
pub struct Sweeps {
    last_high: bool,
    /// Rising edges not yet cut.
    pending: VecDeque<u64>,
    /// Recent (rise, fall) pairs, for the laser-on shading of the newest sweep.
    edges: VecDeque<(u64, Option<u64>)>,
    /// Sweeps cut since the last clear.
    count: u64,
    newest_rise: Option<u64>,
}

impl Sweeps {
    /// Forget all sweeps (window, line or slot count changed, or the display restarted).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.edges.clear();
        self.count = 0;
        self.newest_rise = None;
    }

    /// Drop triggers waiting for data that will never be ingested (display backlog trimmed).
    pub fn drop_pending(&mut self) {
        self.pending.clear();
    }

    /// Scan TTL words; `start` is the absolute index of `words[0]`.
    pub fn scan(&mut self, words: &[u16], start: u64, line: usize) {
        let bit = 1u16 << line.min(15);
        for (i, &w) in words.iter().enumerate() {
            let high = w & bit != 0;
            let a = start + i as u64;
            if high && !self.last_high {
                self.pending.push_back(a);
                self.edges.push_back((a, None));
                while self.edges.len() > 64 {
                    self.edges.pop_front();
                }
            } else if !high
                && self.last_high
                && let Some(e) = self.edges.back_mut()
                && e.1.is_none()
            {
                e.1 = Some(a);
            }
            self.last_high = high;
        }
    }

    /// Triggers whose window `[rise - pre, rise + post)` is complete in the GPU ring once
    /// `uploaded` samples have been ingested. Returns (slot, ring index of window start), at
    /// most `max_cuts` (the newest).
    pub fn take_ready(&mut self, uploaded: u64, pre: u32, post: u32, nslots: u32, ring: u32, max_cuts: usize) -> Vec<(u32, u32)> {
        let mut ready = Vec::new();
        while let Some(&rise) = self.pending.front() {
            if rise + post as u64 > uploaded {
                break;
            }
            self.pending.pop_front();
            // Window start must still be in the ring (not yet overwritten).
            if rise >= pre as u64 && rise - pre as u64 + ring as u64 > uploaded {
                ready.push(rise);
            }
        }
        if ready.len() > max_cuts {
            ready.drain(..ready.len() - max_cuts);
        }
        let nslots = nslots.max(1) as u64;
        ready
            .into_iter()
            .map(|rise| {
                let slot = (self.count % nslots) as u32;
                self.count += 1;
                self.newest_rise = Some(rise);
                (slot, ((rise - pre as u64) % ring as u64) as u32)
            })
            .collect()
    }

    pub fn newest_slot(&self, nslots: u32) -> u32 {
        if self.count == 0 { 0 } else { ((self.count - 1) % nslots.max(1) as u64) as u32 }
    }
    pub fn filled(&self, nslots: u32) -> u32 {
        self.count.min(nslots as u64) as u32
    }
    pub fn count(&self) -> u64 {
        self.count
    }
    /// Pulse width of the newest sweep in samples (None = still high / unknown).
    pub fn newest_width(&self) -> Option<u64> {
        let r = self.newest_rise?;
        self.edges.iter().find(|e| e.0 == r).and_then(|e| e.1.map(|f| f - r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_after_post_window_with_slots_and_width() {
        let mut s = Sweeps::default();
        // Line 1 (TTL2): high for samples 100..120 and 300..320.
        let words: Vec<u16> = (0..400u64).map(|i| if (100..120).contains(&i) || (300..320).contains(&i) { 0b10 } else { 0 }).collect();
        s.scan(&words[..200], 0, 1);
        // pre 10, post 50: first trigger needs samples up to 150.
        assert_eq!(s.take_ready(140, 10, 50, 4, 1024, 16), vec![]);
        assert_eq!(s.take_ready(200, 10, 50, 4, 1024, 16), vec![(0, 90)]);
        assert_eq!(s.newest_width(), Some(20));
        s.scan(&words[200..], 200, 1);
        assert_eq!(s.take_ready(400, 10, 50, 4, 1024, 16), vec![(1, 290)]);
        assert_eq!((s.newest_slot(4), s.filled(4)), (1, 2));
    }

    #[test]
    fn drops_triggers_overwritten_in_ring_and_wraps_slots() {
        let mut s = Sweeps::default();
        let mut words = vec![0u16; 5000];
        for r in [100usize, 1100, 2100, 3100, 4100] {
            words[r] = 1;
        }
        s.scan(&words, 0, 0);
        // Ring of 2048 samples, everything uploaded at once: only windows starting after
        // 5000 - 2048 survive (rises 3100 and 4100).
        let cuts = s.take_ready(5000, 10, 50, 2, 2048, 16);
        assert_eq!(cuts, vec![(0, (3090 % 2048) as u32), (1, (4090 % 2048) as u32)]);
        assert_eq!(s.filled(2), 2);
        assert_eq!(s.newest_slot(2), 1);
    }
}
