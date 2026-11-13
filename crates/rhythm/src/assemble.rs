//! Block assembly: Rhythm frames from the board's device stream into contiguous blocks.
//!
//! This is the acquisition hot path, shared by `OniBoard::read_block` (frames from liboni)
//! and the deterministic simulation test (`crates/engine/tests/simulation.rs`, frames with
//! seeded faults), so the test exercises exactly the code that runs on the board.

use crate::oni_board::DEVICE_RHYTHM;
use crate::parser::{ParseResult, RhythmParser};
use crate::types::{Block, Health};
use std::sync::atomic::Ordering;

/// A frame of the board's device stream: liboni's `oni::Frame`, or a simulated one.
pub trait DeviceFrame {
    fn dev_idx(&self) -> u32;
    /// Acquisition-clock ticks.
    fn time(&self) -> u64;
    /// Payload, starting with the 8-byte hub timestamp.
    fn data(&self) -> &[u8];
}

impl DeviceFrame for crate::oni::Frame {
    fn dev_idx(&self) -> u32 {
        crate::oni::Frame::dev_idx(self)
    }
    fn time(&self) -> u64 {
        crate::oni::Frame::time(self)
    }
    fn data(&self) -> &[u8] {
        crate::oni::Frame::data(self)
    }
}

/// Why `assemble` returned.
pub enum Assembled<F> {
    /// The block has `target` samples, or the next frame isn't contiguous with it (that
    /// frame is kept in `pending` and starts the next block).
    Block,
    /// A frame from another device (memory monitor, BNO); the caller handles it and calls
    /// again to continue the same block.
    Side(F),
}

/// Reads frames with `next` (after the pending one, if any) and parses Rhythm frames into
/// `blk` until it holds `target` samples or a gap ends it. `blk` is not cleared here.
/// Every frame read counts in `health.frames`; bad frames are skipped (counted by the parser).
#[allow(clippy::too_many_arguments)]
pub fn assemble<F: DeviceFrame, E>(
    parser: &mut RhythmParser,
    pending: &mut Option<F>,
    blk: &mut Block,
    target: usize,
    has_rhythm: bool,
    clock_hz: u32,
    health: &Health,
    mut next: impl FnMut() -> Result<F, E>,
) -> Result<Assembled<F>, E> {
    loop {
        let f = match pending.take() {
            Some(f) => f,
            None => next()?,
        };
        health.frames.fetch_add(1, Ordering::Relaxed);
        if f.dev_idx() != DEVICE_RHYTHM {
            return Ok(Assembled::Side(f));
        }
        if !has_rhythm {
            continue;
        }
        let ts = f.time() as f64 / clock_hz.max(1) as f64;
        match parser.parse(&f.data()[8.min(f.data().len())..], ts, blk, health) {
            ParseResult::Discontinuous => {
                *pending = Some(f);
                return Ok(Assembled::Block);
            }
            ParseResult::Appended | ParseResult::Bad => {}
        }
        if blk.len() >= target {
            return Ok(Assembled::Block);
        }
    }
}
