//! Pure parser for Rhythm sample frames (the ONI `DEVICE_RHYTHM` payload).
//!
//! Layout after the 8-byte ONI hub timestamp (all little-endian u16 unless noted):
//! ```text
//! magic u64 | counter u32 | aux[3][ns] | amp[32][ns] | filler[ns] | adc[8] | ttl_in | ttl_out
//! ```
//! (`[a][b]` = outer a, inner b, i.e. stream index varies fastest.)

use crate::types::{Block, Health};
use std::sync::atomic::Ordering;

pub const RHD2000_MAGIC: u64 = 0xc691199927021942;
pub const CHIP_ID_RHD2164_B: i32 = 1000;
pub const ADC_BIT_VOLTS: f64 = 0.00015258789;
/// First chip channel recorded from an RHD2132 used as a 16-channel headstage (stock
/// plugin: `RHD2132_16CH_OFFSET`). Such headstages wire chip channels 8–23.
pub const RHD2132_16CH_OFFSET: usize = 8;

/// First chip channel read from a stream: 8 for an RHD2132 in 16-channel mode, else 0.
/// Same rule as the stock plugin (`chipId == RHD2132 && nChans == 16`).
pub fn first_chip_channel(chip_id: i32, channels: usize) -> usize {
    if chip_id == crate::oni_board::CHIP_ID_RHD2132 && channels == 16 { RHD2132_16CH_OFFSET } else { 0 }
}

#[derive(Clone, Debug, Default)]
pub struct RhythmParser {
    pub channels_per_stream: Vec<usize>,
    pub chip_ids: Vec<i32>,
    pub acquire_aux: bool,
    pub acquire_adc: bool,
    pub adc_v3: bool,
    aux_samples: Vec<[i16; 3]>,
    aux_hold: Vec<i16>,
    last_counter: Option<u32>,
    counter_hi: i64,
    /// Consecutive frames rejected for a repeated or backwards counter.
    behind: u32,
}

/// After this many consecutive frames with a counter behind the last one, the counter is taken
/// to have restarted (0.1 s at 30 kHz): sample numbers carry on from the last one.
const RESYNC_AFTER: u32 = 3000;

pub enum ParseResult {
    /// Sample appended to the block.
    Appended,
    /// Frame starts a new contiguous run; flush the block and re-submit the frame.
    Discontinuous,
    /// Malformed frame (bad magic/size); skipped.
    Bad,
}

impl RhythmParser {
    pub fn new(channels_per_stream: Vec<usize>, chip_ids: Vec<i32>, acquire_aux: bool, acquire_adc: bool, adc_v3: bool) -> Self {
        let mut p = Self { channels_per_stream, chip_ids, acquire_aux, acquire_adc, adc_v3, ..Default::default() };
        p.reset();
        p
    }
    pub fn reset(&mut self) {
        self.aux_samples = vec![[0; 3]; self.channels_per_stream.len()];
        self.aux_hold = vec![0; self.num_aux()];
        self.last_counter = None;
        self.counter_hi = 0;
    }
    pub fn num_streams(&self) -> usize {
        self.channels_per_stream.len()
    }
    pub fn num_aux(&self) -> usize {
        if self.acquire_aux { 3 * self.chip_ids.iter().filter(|&&c| c != CHIP_ID_RHD2164_B).count() } else { 0 }
    }
    fn first_chip_channel(&self, s: usize) -> usize {
        first_chip_channel(self.chip_ids[s], self.channels_per_stream[s])
    }
    pub fn num_channels(&self) -> usize {
        self.channels_per_stream.iter().sum::<usize>() + self.num_aux() + if self.acquire_adc { 8 } else { 0 }
    }
    /// Payload size in bytes (excluding the 8-byte hub timestamp).
    pub fn payload_len(&self) -> usize {
        let ns = self.num_streams();
        12 + 2 * (3 * ns + 32 * ns + ns + 8 + 2)
    }

    /// `d` is the Rhythm payload (frame data with the 8-byte hub timestamp removed).
    pub fn parse(&mut self, d: &[u8], timestamp_s: f64, blk: &mut Block, health: &Health) -> ParseResult {
        if d.len() < self.payload_len() || u64::from_le_bytes(d[0..8].try_into().unwrap()) != RHD2000_MAGIC {
            health.bad_frames.fetch_add(1, Ordering::Relaxed);
            return ParseResult::Bad;
        }
        let counter = u32::from_le_bytes(d[8..12].try_into().unwrap());
        let mut delta = self.last_counter.map(|last| counter.wrapping_sub(last));
        // A repeated or backwards counter (a forward jump of over 2^31 samples, 20 h at 30 kHz,
        // can't be a real gap): a duplicated or stale frame. Skip it as a bad frame; taken as
        // a gap it would add ~4.3e9 to "lost" and shift every later sample number by 2^32.
        if let Some(dl) = delta
            && (dl == 0 || dl > 1 << 31)
        {
            if self.behind < RESYNC_AFTER {
                self.behind += 1;
                health.bad_frames.fetch_add(1, Ordering::Relaxed);
                return ParseResult::Bad;
            }
            // Still behind after RESYNC_AFTER frames: the counter restarted. Carry on from
            // the next sample number (a counted discontinuity, nothing dropped).
            delta = Some(1);
            health.discontinuities.fetch_add(1, Ordering::Relaxed);
        }
        let sample_number = match delta {
            None => 0,
            Some(dl) => self.counter_hi + dl as i64,
        };
        if !blk.is_empty() && sample_number != blk.first_sample + blk.len() as i64 {
            return ParseResult::Discontinuous;
        }
        if let Some(dl) = delta
            && dl != 1
        {
            health.discontinuities.fetch_add(1, Ordering::Relaxed);
            health.dropped_samples.fetch_add(dl.wrapping_sub(1) as u64, Ordering::Relaxed);
        }
        self.behind = 0;
        self.last_counter = Some(counter);
        self.counter_hi = sample_number;
        if blk.is_empty() {
            blk.first_sample = sample_number;
        }

        let ns = self.num_streams();
        let word = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let aux_base = 12usize;
        let amp_base = aux_base + 6 * ns;
        for s in 0..ns {
            let mut o = amp_base + 2 * s + 2 * ns * self.first_chip_channel(s);
            for _ in 0..self.channels_per_stream[s] {
                blk.data.push((word(o) as i32 - 32768) as i16);
                o += 2 * ns;
            }
        }
        if self.acquire_aux {
            // AuxCmd2 cycles AuxIn1..3 + one other command; values are held between updates
            // exactly as the stock plugin does.
            let aux_num = (counter.wrapping_add(3) % 4) as usize;
            let mut ai = aux_base + 2 * ns;
            let mut ch = 0;
            for s in 0..ns {
                if self.chip_ids[s] != CHIP_ID_RHD2164_B {
                    if aux_num < 3 {
                        self.aux_samples[s][aux_num] = (word(ai) as i32 - 32768) as i16;
                    }
                    for k in 0..3 {
                        if aux_num == 3 {
                            self.aux_hold[ch] = self.aux_samples[s][k];
                        }
                        blk.data.push(self.aux_hold[ch]);
                        ch += 1;
                    }
                }
                ai += 2;
            }
        }
        let adc_base = amp_base + 64 * ns + 2 * ns;
        if self.acquire_adc {
            for k in 0..8 {
                let raw = word(adc_base + 2 * k) as f64;
                let volts = if self.adc_v3 { -(ADC_BIT_VOLTS * raw - 5.0) } else { ADC_BIT_VOLTS * raw - 5.0 - 0.4096 };
                blk.data.push((volts / ADC_BIT_VOLTS).round().clamp(-32768.0, 32767.0) as i16);
            }
        }
        blk.ttl.push(word(adc_base + 16));
        blk.timestamps.push(timestamp_s);
        health.samples.fetch_add(1, Ordering::Relaxed);
        ParseResult::Appended
    }
}

/// Build a synthetic Rhythm payload (used by tests and the hardware-free self check).
pub fn encode_payload(ns: usize, counter: u32, aux: impl Fn(usize, usize) -> u16, amp: impl Fn(usize, usize) -> u16, adc: [u16; 8], ttl: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&RHD2000_MAGIC.to_le_bytes());
    v.extend_from_slice(&counter.to_le_bytes());
    for c in 0..3 {
        for s in 0..ns {
            v.extend_from_slice(&aux(s, c).to_le_bytes());
        }
    }
    for c in 0..32 {
        for s in 0..ns {
            v.extend_from_slice(&amp(s, c).to_le_bytes());
        }
    }
    v.extend(std::iter::repeat_n(0u8, 2 * ns));
    for a in adc {
        v.extend_from_slice(&a.to_le_bytes());
    }
    v.extend_from_slice(&ttl.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amp_val(s: usize, c: usize) -> u16 {
        (32768 + 100 * s as i32 + c as i32) as u16
    }

    #[test]
    fn maps_streams_and_channels_like_upstream() {
        // Three streams: RHD2132 (32ch), RHD2164 A+B (32+32).
        let mut p = RhythmParser::new(vec![32, 32, 32], vec![1, 4, CHIP_ID_RHD2164_B], false, true, false);
        let h = Health::default();
        let mut b = Block::default();
        b.num_channels = p.num_channels();
        let d = encode_payload(3, 7, |_, _| 0, amp_val, [0, 32768, 65535, 0, 0, 0, 0, 0], 0b1010_0101);
        assert_eq!(d.len(), p.payload_len());
        assert!(matches!(p.parse(&d, 0.0, &mut b, &h), ParseResult::Appended));
        assert_eq!(b.data.len(), 96 + 8);
        for s in 0..3 {
            for c in 0..32 {
                assert_eq!(b.data[s * 32 + c], (100 * s + c) as i16);
            }
        }
        // ADC: raw 32768 -> 0 V - 0.4096 V on non-v3 boards.
        let v = b.data[96 + 1] as f64 * ADC_BIT_VOLTS;
        assert!((v + 0.4096).abs() < 1e-3, "{v}");
        assert_eq!(b.ttl[0], 0b1010_0101);
    }

    #[test]
    fn rhd2132_16ch_reads_chip_channels_8_to_23() {
        // Stream 0: RHD2132 in 16-channel mode; stream 1: RHD2216 (16 ch, no offset).
        let mut p = RhythmParser::new(vec![16, 16], vec![1, 2], false, false, false);
        let h = Health::default();
        let mut b = Block { num_channels: p.num_channels(), ..Default::default() };
        assert_eq!(p.num_channels(), 32);
        let d = encode_payload(2, 0, |_, _| 0, amp_val, [0; 8], 0);
        assert_eq!(d.len(), p.payload_len());
        assert!(matches!(p.parse(&d, 0.0, &mut b, &h), ParseResult::Appended));
        for c in 0..16 {
            assert_eq!(b.data[c], (8 + c) as i16, "RHD2132 16-ch slot {c}");
            assert_eq!(b.data[16 + c], (100 + c) as i16, "RHD2216 slot {c}");
        }
        assert_eq!(first_chip_channel(1, 32), 0);
        assert_eq!(first_chip_channel(2, 16), 0);
        assert_eq!(first_chip_channel(1, 16), 8);
    }

    #[test]
    fn aux_sample_and_hold_skips_2164b() {
        let mut p = RhythmParser::new(vec![32, 32], vec![4, CHIP_ID_RHD2164_B], true, false, false);
        let h = Health::default();
        let mut b = Block::default();
        b.num_channels = p.num_channels();
        // counters 1,2,3 carry aux 0,1,2 (aux_num = (c+3)%4), counter 4 latches them.
        for c in 1..=4u32 {
            let d = encode_payload(2, c, |s, slot| if slot == 1 { (32768 + 10 * (c as i32) + s as i32) as u16 } else { 0 }, |_, _| 32768, [0; 8], 0);
            assert!(matches!(p.parse(&d, 0.0, &mut b, &h), ParseResult::Appended));
        }
        assert_eq!(p.num_aux(), 3);
        let last = b.sample(3);
        assert_eq!(&last[64..67], &[10, 20, 30]);
    }

    #[test]
    fn detects_gaps_without_double_counting() {
        let mut p = RhythmParser::new(vec![32], vec![1], false, false, false);
        let h = Health::default();
        let mut b = Block::default();
        let f = |c| encode_payload(1, c, |_, _| 0, |_, _| 32768, [0; 8], 0);
        assert!(matches!(p.parse(&f(100), 0.0, &mut b, &h), ParseResult::Appended));
        assert!(matches!(p.parse(&f(101), 0.0, &mut b, &h), ParseResult::Appended));
        // Gap: 102..104 missing.
        assert!(matches!(p.parse(&f(105), 0.0, &mut b, &h), ParseResult::Discontinuous));
        assert_eq!(h.dropped_samples.load(Ordering::Relaxed), 0);
        let mut b2 = Block::default();
        assert!(matches!(p.parse(&f(105), 0.0, &mut b2, &h), ParseResult::Appended));
        assert_eq!(b2.first_sample, 5);
        assert_eq!(h.dropped_samples.load(Ordering::Relaxed), 3);
        assert_eq!(h.discontinuities.load(Ordering::Relaxed), 1);
        // Counter wrap-around is continuous.
        let mut p = RhythmParser::new(vec![32], vec![1], false, false, false);
        let mut b = Block::default();
        assert!(matches!(p.parse(&f(u32::MAX), 0.0, &mut b, &h), ParseResult::Appended));
        assert!(matches!(p.parse(&f(0), 0.0, &mut b, &h), ParseResult::Appended));
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn skips_repeated_or_backwards_counters_and_resyncs_after_a_restart() {
        let mut p = RhythmParser::new(vec![32], vec![1], false, false, false);
        let h = Health::default();
        let mut b = Block::default();
        let f = |c| encode_payload(1, c, |_, _| 0, |_, _| 32768, [0; 8], 0);
        for c in [1_000_000, 1_000_001] {
            assert!(matches!(p.parse(&f(c), 0.0, &mut b, &h), ParseResult::Appended));
        }
        // A duplicate and a stale frame: skipped, nothing counted as lost.
        assert!(matches!(p.parse(&f(1_000_001), 0.0, &mut b, &h), ParseResult::Bad));
        assert!(matches!(p.parse(&f(999_990), 0.0, &mut b, &h), ParseResult::Bad));
        assert!(matches!(p.parse(&f(1_000_002), 0.0, &mut b, &h), ParseResult::Appended));
        assert_eq!((h.dropped_samples.load(Ordering::Relaxed), h.bad_frames.load(Ordering::Relaxed)), (0, 2));
        assert_eq!((b.first_sample, b.len()), (0, 3));
        // The counter restarts at 0 for good: after RESYNC_AFTER skipped frames, sample
        // numbers carry on from the last one.
        let mut c = 0;
        while matches!(p.parse(&f(c), 0.0, &mut b, &h), ParseResult::Bad) {
            c += 1;
        }
        assert_eq!(c, RESYNC_AFTER);
        assert_eq!((b.first_sample, b.len()), (0, 4), "the restarted frame is sample 3");
        assert!(matches!(p.parse(&f(c + 1), 0.0, &mut b, &h), ParseResult::Appended));
        assert_eq!(h.dropped_samples.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut p = RhythmParser::new(vec![32], vec![1], false, false, false);
        let h = Health::default();
        let mut b = Block::default();
        let mut d = encode_payload(1, 0, |_, _| 0, |_, _| 0, [0; 8], 0);
        d[0] ^= 1;
        assert!(matches!(p.parse(&d, 0.0, &mut b, &h), ParseResult::Bad));
        assert!(b.is_empty());
    }
}
