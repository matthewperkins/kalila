//! Slope (level-crossing) trigger for sweeps, computed per trace window on the CPU.
//!
//! One channel is band-passed with the spike detector's band (300–6k, same CAR) and
//! triggers when it crosses a level with the chosen slope. The level is the spike detector's
//! threshold for that channel by default, so the sweeps match the spike tiles, or a fixed µV
//! set by dragging the line. Triggers are aligned on the trough (the extreme within 0.5 ms
//! of the crossing, as the spike detector does) or on the crossing itself. A dead time of
//! 1 ms plus an optional holdoff follows every crossing. An optional window discriminator
//! rejects crossings whose trough goes beyond a second level.

use crate::config::{Slope, TrigAlign};
use engine::dsp::{BandSpec, FilterBank};

/// Settings of one scan (all in µV and samples).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlopeParams {
    /// Crossing level; negative for a negative slope, positive for a positive one.
    pub level: f32,
    pub slope: Slope,
    pub align: TrigAlign,
    /// Samples after a crossing during which no new trigger starts (dead time included).
    pub holdoff: u32,
    /// Window discriminator: the trough must not go beyond this level (same sign as `level`).
    pub window: Option<f32>,
}

pub struct SlopeTrigger {
    bank: FilterBank,
    fs: f64,
    prev: f32,
    hold: u32,
    /// (crossing sample, extreme's sample, extreme's value, samples left in the alignment window).
    pend: Option<(u64, u64, f32, u32)>,
    /// Samples needed for the filter to settle after a reset.
    warm: u32,
}

/// Alignment window after the crossing (s), as in `engine::dsp::SpikeDetector`.
const ALIGN_S: f64 = 0.5e-3;
/// Dead time after a crossing (s), as in the spike detector.
pub const DEAD_S: f64 = 1.0e-3;

impl SlopeTrigger {
    pub fn new(fs: f64) -> Self {
        Self { bank: FilterBank::new(BandSpec::SPIKES.sections(fs), 1), fs, prev: 0.0, hold: 0, pend: None, warm: 0 }
    }

    /// Start afresh (channel or data continuity changed).
    pub fn reset(&mut self) {
        self.bank = FilterBank::new(BandSpec::SPIKES.sections(self.fs), 1);
        self.prev = 0.0;
        self.hold = 0;
        self.pend = None;
        self.warm = (self.fs * 0.05) as u32;
    }

    pub fn align_window(&self) -> u32 {
        ((self.fs * ALIGN_S) as u32).max(1)
    }

    /// Feed one raw sample in µV (CAR already applied) with absolute index `a`; completed
    /// triggers are appended to `out` (aligned sample indices, in order).
    pub fn feed(&mut self, x_uv: f32, a: u64, p: &SlopeParams, out: &mut Vec<u64>) {
        let x = self.bank.tick(0, x_uv);
        let prev = std::mem::replace(&mut self.prev, x);
        if self.warm > 0 {
            self.warm -= 1;
            return;
        }
        if self.hold > 0 {
            self.hold -= 1;
        }
        let neg = p.slope == Slope::Neg;
        if let Some((cross, at, best, left)) = self.pend.as_mut() {
            if (neg && x < *best) || (!neg && x > *best) {
                *best = x;
                *at = a;
            }
            *left -= 1;
            if *left == 0 {
                let ok = match p.window {
                    Some(w) if neg => *best > w,
                    Some(w) => *best < w,
                    None => true,
                };
                if ok {
                    out.push(match p.align {
                        TrigAlign::Trough => *at,
                        TrigAlign::Crossing => *cross,
                    });
                }
                self.pend = None;
            }
            return;
        }
        let crossed = if neg { prev >= p.level && x < p.level && p.level < 0.0 } else { prev <= p.level && x > p.level && p.level > 0.0 };
        if crossed && self.hold == 0 {
            self.pend = Some((a, a, x, self.align_window()));
            self.hold = p.holdoff.max(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &SlopeParams, wave: impl Fn(u64) -> f32, n: u64) -> Vec<u64> {
        let mut t = SlopeTrigger::new(30000.0);
        let mut out = Vec::new();
        for a in 0..n {
            t.feed(wave(a), a, p, &mut out);
        }
        out
    }

    /// A spike at `at`: five samples, extreme on the middle one. The 300–6k band delays it
    /// by `LAG` samples (the 6 kHz low-pass's group delay), as in the spike detector.
    fn spike(a: u64, at: u64, amp: f32) -> f32 {
        match a as i64 - at as i64 {
            -2 | 2 => amp * 0.3,
            -1 | 1 => amp * 0.7,
            0 => amp,
            _ => 0.0,
        }
    }

    const LAG: u64 = 2;

    #[test]
    fn negative_slope_aligns_on_trough_or_crossing() {
        let p = SlopeParams { level: -50.0, slope: Slope::Neg, align: TrigAlign::Trough, holdoff: 30, window: None };
        let out = run(&p, |a| spike(a, 10000, -120.0), 12000);
        assert_eq!(out, vec![10000 + LAG]);
        let p = SlopeParams { align: TrigAlign::Crossing, ..p };
        let out = run(&p, |a| spike(a, 10000, -120.0), 12000);
        assert_eq!(out, vec![10000 + LAG - 1], "the crossing is on the shoulder before the trough");
    }

    #[test]
    fn holdoff_window_and_positive_slope() {
        // Two spikes 20 samples apart: the second is inside a 1 ms holdoff.
        let p = SlopeParams { level: -50.0, slope: Slope::Neg, align: TrigAlign::Trough, holdoff: 30, window: None };
        let w = |a| spike(a, 10000, -120.0) + spike(a, 10020, -120.0) + spike(a, 10100, -120.0);
        assert_eq!(run(&p, w, 12000), vec![10000 + LAG, 10100 + LAG]);
        // Window discriminator: a −400 µV spike goes beyond −200 and is rejected.
        let p = SlopeParams { window: Some(-200.0), ..p };
        let w = |a| spike(a, 10000, -120.0) + spike(a, 10100, -400.0);
        assert_eq!(run(&p, w, 12000), vec![10000 + LAG]);
        // Positive slope ignores a negative spike (whose band-passed rebound stays under the level).
        let p = SlopeParams { level: 50.0, slope: Slope::Pos, align: TrigAlign::Trough, holdoff: 30, window: None };
        let w = |a| spike(a, 10000, -60.0) + spike(a, 10100, 120.0);
        assert_eq!(run(&p, w, 12000), vec![10100 + LAG]);
    }

    #[test]
    fn no_trigger_while_warming_up_or_at_a_zero_level() {
        let p = SlopeParams { level: -50.0, slope: Slope::Neg, align: TrigAlign::Trough, holdoff: 30, window: None };
        let mut t = SlopeTrigger::new(30000.0);
        t.reset();
        let mut out = Vec::new();
        for a in 0..3000 {
            t.feed(spike(a, 100, -120.0), a, &p, &mut out);
        }
        assert!(out.is_empty(), "a spike during the filter's settling time is ignored");
        let p = SlopeParams { level: 0.0, ..p };
        assert!(run(&p, |a| spike(a, 10000, -120.0), 12000).is_empty(), "no level yet (noise estimate pending)");
    }
}
