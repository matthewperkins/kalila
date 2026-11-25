//! The driver against a scripted FT600 (`crate::fake`), through the same C function table
//! liboni calls: what it sends on open and close, register access through the control stream,
//! data in any chunking, faults, and pacing.
//!
//! Seeded tests run a few hundred seeds on all cores; a failure prints its seeds, and
//! `KALILA_FT600_SEED=<n> cargo test --release -p ft600 fake_tests` reruns one seed in each
//! seeded test.

use std::collections::HashMap;
use std::os::raw::c_int;
use std::sync::Mutex;

use super::*;
use crate::fake::{self, Fake, Fault, Policy, Rng};

/// A driver context opened on an installed fake.
struct Board {
    p: sys::oni_driver_ctx,
    fake: fake::Handle,
}

impl Board {
    fn open(f: Fake) -> (Board, c_int) {
        Self::open_at(f, -1)
    }

    fn open_at(f: Fake, host_idx: c_int) -> (Board, c_int) {
        let fake = fake::install(f);
        let p = create_ctx();
        let rc = init(p, host_idx);
        (Board { p, fake }, rc)
    }

    fn ctx(&mut self) -> &mut Ctx {
        unsafe { cast(self.p) }
    }

    fn fake(&self) -> std::cell::RefMut<'_, Fake> {
        self.fake.borrow_mut()
    }

    fn opt(&mut self, option: c_int, v: u32) -> c_int {
        set_opt_callback(self.p, option, (&v as *const u32).cast(), 4)
    }

    /// Starts acquisition with pacing off: its sleeps are real time while the fake's
    /// transfers take none, so it would learn a rate from the sleeps themselves and slow the
    /// tests down. `pacing_changes_nothing_but_when` turns it on.
    fn start(&mut self, block: u32) -> c_int {
        let rc = match self.opt(sys::ONI_OPT_BLOCKREADSIZE, block) {
            0 => self.opt(sys::ONI_OPT_RUNNING, 1),
            e => e,
        };
        self.ctx().pace = false;
        rc
    }

    fn stop(&mut self) -> c_int {
        self.opt(sys::ONI_OPT_RUNNING, 0)
    }

    fn write(&mut self, reg: sys::oni_config_t, v: u32) -> c_int {
        write_config(self.p, reg, v)
    }

    fn read(&mut self, reg: sys::oni_config_t) -> Result<u32, c_int> {
        let mut v = 0;
        match read_config(self.p, reg, &mut v) {
            0 => Ok(v),
            e => Err(e),
        }
    }

    /// A device register write, as liboni makes one.
    fn dev_write(&mut self, idx: u32, addr: u32, v: u32) -> c_int {
        for (reg, v) in [
            (sys::ONI_CONFIG_DEV_IDX, idx),
            (sys::ONI_CONFIG_REG_ADDR, addr),
            (sys::ONI_CONFIG_REG_VALUE, v),
            (sys::ONI_CONFIG_RW, 1),
            (sys::ONI_CONFIG_TRIG, 1),
        ] {
            let rc = self.write(reg, v);
            if rc != 0 {
                return rc;
            }
        }
        0
    }

    /// A device register read: the access, then the value from REG_VALUE.
    fn dev_read(&mut self, idx: u32, addr: u32) -> Result<u32, c_int> {
        for (reg, v) in [
            (sys::ONI_CONFIG_DEV_IDX, idx),
            (sys::ONI_CONFIG_REG_ADDR, addr),
            (sys::ONI_CONFIG_RW, 0),
            (sys::ONI_CONFIG_TRIG, 1),
        ] {
            let rc = self.write(reg, v);
            if rc != 0 {
                return Err(rc);
            }
        }
        self.read(sys::ONI_CONFIG_REG_VALUE)
    }

    fn read_data(&mut self, buf: &mut [u8]) -> c_int {
        read_stream(self.p, sys::ONI_READ_STREAM_DATA, buf.as_mut_ptr().cast(), buf.len())
    }

    fn read_signal(&mut self, buf: &mut [u8]) -> c_int {
        read_stream(self.p, sys::ONI_READ_STREAM_SIGNAL, buf.as_mut_ptr().cast(), buf.len())
    }

    fn write_data(&mut self, data: &[u8]) -> c_int {
        write_stream(self.p, sys::ONI_WRITE_STREAM_DATA, data.as_ptr().cast(), data.len())
    }

    /// Destroys the driver context; the fake stays readable.
    fn close(mut self) -> fake::Handle {
        assert_eq!(destroy_ctx(self.p), sys::ONI_ESUCCESS);
        self.p = std::ptr::null_mut();
        self.fake.clone()
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        destroy_ctx(self.p); // null after `close`: nothing to do
        fake::uninstall();
    }
}

/// What every run must end with: the protocol kept, nothing spun, interfaces released.
fn verdict(f: &Fake) -> Result<(), String> {
    if !f.violations.is_empty() {
        let first: Vec<_> = f.violations.iter().take(3).collect();
        return Err(format!("{} protocol violations, first {first:?}", f.violations.len()));
    }
    if f.spun {
        return Err("the driver went on retrying a transfer that moved nothing".into());
    }
    if f.claimed != [false; 2] {
        return Err(format!("interfaces still claimed after close: {:?}", f.claimed));
    }
    Ok(())
}

fn panics() -> u32 {
    PANICS.with(|n| n.get())
}

fn seeds(n: u64) -> Vec<u64> {
    match std::env::var("KALILA_FT600_SEED") {
        Ok(s) => vec![s.parse().expect("KALILA_FT600_SEED")],
        Err(_) => (0..n).collect(),
    }
}

/// Runs `f` for every seed, on all cores (the fake is per thread), and fails with the list of
/// failed seeds. A panic in the driver (caught by `guard`) fails the seed too.
fn run_seeds(n: u64, f: fn(u64) -> Result<(), String>) {
    let seeds = seeds(n);
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for w in 0..workers {
            let (seeds, failures) = (&seeds, &failures);
            s.spawn(move || {
                for &seed in seeds.iter().skip(w).step_by(workers) {
                    let p0 = panics();
                    let r = std::panic::catch_unwind(|| f(seed)).unwrap_or_else(|e| {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default();
                        Err(format!("test panicked: {msg}"))
                    });
                    let r = r.and_then(|()| {
                        if panics() != p0 {
                            Err("the driver panicked".into())
                        } else {
                            Ok(())
                        }
                    });
                    fake::uninstall();
                    if let Err(e) = r {
                        failures.lock().unwrap().push((seed, e));
                    }
                }
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.sort();
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed (rerun one with KALILA_FT600_SEED=<n>):\n{}",
        failures.len(),
        seeds.len(),
        failures
            .iter()
            .take(20)
            .map(|(s, e)| format!("seed {s}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Register values, often made of bytes that look like control-stream markers (1, 2) or ends (0).
fn value(rng: &mut Rng) -> u32 {
    if rng.chance(500) {
        rng.next() as u32
    } else {
        let mut b = [0u8; 4];
        for x in &mut b {
            *x = [0, 1, 2, 0xff][rng.below(4) as usize];
        }
        u32::from_le_bytes(b)
    }
}

/// What the fake board answers for a device register never written.
fn unwritten(addr: u32) -> u32 {
    addr.wrapping_mul(0x9E37_79B9)
}

fn fake_for(seed: u64, rng: &mut Rng) -> Fake {
    let mut f = Fake::new(seed, Policy::random(rng));
    if rng.chance(300) {
        f.loc.speed = 480; // high speed: 512-byte packets
    }
    f
}

// ---------------------------------------------------------------- 1. open and close

/// Everything the driver sends from `init` to `destroy_ctx`, in order, with its timeouts.
/// A change to what goes to the chip shows up here. Read against `Ctx::init`,
/// `Ft600::open`, `Ctx::reset_acq` and `destroy_ctx`; the 20-byte commands are
/// `index u32 | pipe u8 | command u8 | 0 u16 | length u32 | 0 u32 | 0 u32`.
const OPEN_CLOSE: &str = "\
claim 0
claim 1
control c0 f1 0000 0000 in 4 t1000 -> 4
bulk 01 0000000082030000000000000000000000000000 t1000 -> 20  # abort 82
bulk 01 0000000083030000000000000000000000000000 t1000 -> 20  # abort 83
control c0 cf 0001 0000 in 152 t1000 -> 152
control 40 02 0003 0000 out 2d00 t1000 -> 2
control 40 02 0001 0000 out 0400 t1000 -> 2
control 40 02 0002 0000 out 0500 t1000 -> 2
control 40 02 0001 0000 out 0500 t1000 -> 2
bulk 01 0000000082030000000000000000000000000000 t1000 -> 20  # abort 82
bulk 01 0000000002030000000000000000000000000000 t1000 -> 20  # abort 02
control 40 02 0001 0000 out 0400 t1000 -> 2
bulk 01 0000000003010000050000000000000000000000 t1000 -> 20  # transfer 03 5
bulk 03 0300000a0a t1000 -> 5
control 40 02 0001 0000 out 0500 t1000 -> 2
bulk 01 0000000082030000000000000000000000000000 t1000 -> 20  # abort 82
bulk 01 0000000002030000000000000000000000000000 t1000 -> 20  # abort 02
bulk 01 0000000003030000000000000000000000000000 t1000 -> 20  # abort 03
bulk 01 0000000083030000000000000000000000000000 t1000 -> 20  # abort 83
release 1
release 0";

#[test]
fn opening_and_closing_send_exactly_this() {
    let mut f = Fake::new(0, Policy::exact());
    f.log = Some(Vec::new());
    let p0 = panics();
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, sys::ONI_ESUCCESS);
    // The board was left out of FIFO reset, GPIO 0 an output, the ONI priority set.
    let gpio = {
        let f = b.fake();
        (f.gpio_level, f.gpio_dir)
    };
    assert_eq!(gpio, (0, 1));
    assert_eq!(b.fake().prio, Some(DEFAULT_PRIO));
    let mut fw = 0u32;
    let mut len = 4usize;
    assert_eq!(get_opt(b.p, 0, (&mut fw as *mut u32).cast(), &mut len), 0);
    assert_eq!(fw, 0x0105);
    assert_eq!(b.ctx().state, State::Init);
    let h = b.close();
    let f = h.borrow();
    let log = f.log.as_ref().unwrap().join("\n");
    assert_eq!(log, OPEN_CLOSE, "the transcript changed; it is now:\n{log}");
    // Destroying leaves the board's FIFO in reset, as upstream does.
    assert_eq!(f.gpio_level, 1);
    verdict(&f).unwrap();
    assert_eq!(panics(), p0);
}

/// A register write, a register read, one data block, a data write, stop: the bytes on the
/// wire, after `init`.
const SESSION: &str = "\
bulk 01 00000000030100002d0000000000000000000000 t1000 -> 20  # transfer 03 45
bulk 03 01000000000300000001010000000001000001030000000100000001020000000c0000c0010400000001000000 t1000 -> 45
bulk 01 0000000003010000050000000000000000000000 t1000 -> 20  # transfer 03 5
bulk 03 0206000000 t1000 -> 5
bulk 01 0000000083010000002000000000000000000000 t1000 -> 20  # transfer 83 8192
bulk 83 [8192] t100 -> 9 015783000200000000
bulk 01 0000000082010000001000000000000000000000 t1000 -> 20  # transfer 82 4096
bulk 82 [4096] t1000 -> 4096
bulk 01 0000000002010000080000000000000000000000 t1000 -> 20  # transfer 02 8
bulk 02 0102030405060708 t1000 -> 8
bulk 01 0000000082030000000000000000000000000000 t1000 -> 20  # abort 82";

#[test]
fn a_register_access_and_a_data_block_send_exactly_this() {
    let mut f = Fake::new(0, Policy::exact());
    f.policy.produce_max = 4096;
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, sys::ONI_ESUCCESS);
    b.fake().log = Some(Vec::new());
    assert_eq!(b.dev_write(3, 0x100, 0xc000_000c), 0);
    assert_eq!(b.read(sys::ONI_CONFIG_RESET), Ok(0));
    assert_eq!(b.start(4096), 0);
    let mut buf = vec![0u8; 4096];
    assert_eq!(b.read_data(&mut buf), 4096);
    assert_eq!(b.write_data(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), 9); // 8 sent: whole words only
    assert_eq!(b.stop(), 0);
    let log = b.fake().log.take().unwrap().join("\n");
    assert_eq!(log, SESSION, "the transcript changed; it is now:\n{log}");
    let h = b.close();
    let f = h.borrow();
    assert_eq!(f.devregs.get(&(3, 0x100)), Some(&0xc000_000c));
    assert_eq!(buf, f.produced[..4096]);
    assert_eq!(f.data_out, [1, 2, 3, 4, 5, 6, 7, 8]);
    verdict(&f).unwrap();
}

// ---------------------------------------------------------------- 2. registers

/// Device and ONI register access through the control stream, with replies split across
/// short reads, timeouts, signal packets and noise in between; and the signal stream handed
/// on whole and in order.
fn registers(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let (mut b, rc) = Board::open(fake_for(seed, &mut rng));
    if rc != 0 {
        return Err(format!("init returned {rc}"));
    }
    let mut model: HashMap<(u32, u32), u32> = HashMap::new();
    let mut consumed = 0;
    let steps = rng.range(20, 150);
    for step in 0..steps {
        let pending = b.fake().signal_sent.len() - consumed;
        let op = if pending > 3000 { 3 } else { rng.below(4) };
        match op {
            0 => {
                let (idx, addr, v) = (rng.below(4) as u32, rng.below(16) as u32, value(&mut rng));
                let rc = b.dev_write(idx, addr, v);
                if rc != 0 {
                    return Err(format!("step {step}: device register write returned {rc}"));
                }
                model.insert((idx, addr), v);
            }
            1 => {
                let (idx, addr) = (rng.below(4) as u32, rng.below(16) as u32);
                let want = model.get(&(idx, addr)).copied().unwrap_or_else(|| unwritten(addr));
                let got = b.dev_read(idx, addr);
                if got != Ok(want) {
                    return Err(format!(
                        "step {step}: device register ({idx}, {addr}) read {got:x?}, not {want:#x}"
                    ));
                }
            }
            2 => {
                let reg = [
                    sys::ONI_CONFIG_RUNNING,
                    sys::ONI_CONFIG_SYSCLKHZ,
                    sys::ONI_CONFIG_ACQCLKHZ,
                    sys::ONI_CONFIG_HWADDRESS,
                ][rng.below(4) as usize];
                let v = value(&mut rng);
                let rc = b.write(reg, v);
                let got = b.read(reg);
                if rc != 0 || got != Ok(v) {
                    return Err(format!(
                        "step {step}: ONI register {reg} written {v:#x} ({rc}), read {got:x?}"
                    ));
                }
            }
            _ => {
                if pending == 0 {
                    continue;
                }
                let n = rng.range(1, pending.min(8191));
                let mut buf = vec![0u8; n];
                let rc = b.read_signal(&mut buf);
                if rc != n as c_int {
                    return Err(format!("step {step}: signal read of {n} returned {rc}"));
                }
                let f = b.fake();
                if buf != f.signal_sent[consumed..consumed + n] {
                    return Err(format!("step {step}: signal stream differs at byte {consumed}"));
                }
                drop(f);
                consumed += n;
            }
        }
    }
    let h = b.close();
    verdict(&h.borrow())
}

#[test]
fn register_access_survives_any_chunking_of_the_control_stream() {
    run_seeds(400, registers);
}

// ---------------------------------------------------------------- 3. data

/// Streams blocks through `read_stream` and returns what the driver handed on and, with
/// `pacing_test`, the transcript. `pace`: pacing on, starting from this many ns per byte.
fn stream(seed: u64, pace: Option<f64>, pacing_test: bool) -> Result<(Vec<u8>, Vec<String>), String> {
    let mut rng = Rng(seed);
    let mut f = fake_for(seed, &mut rng);
    if pacing_test {
        // Pacing sleeps after every short read, in real time: with large chunks and a board
        // that keeps the FIFO full there are few of them.
        f.policy.max_chunk = usize::MAX;
        f.policy.produce_max = f.policy.produce_max.max(16384);
        f.log = Some(Vec::new());
    }
    let (mut b, rc) = Board::open(f);
    if rc != 0 {
        return Err(format!("init returned {rc}"));
    }
    let block = 4 * rng.range(1, if pacing_test { 2048 } else { 6144 }) as u32;
    if b.start(block) != 0 {
        return Err(format!("start with block {block} failed"));
    }
    b.ctx().pace = pace.is_some();
    if let Some(ns) = pace {
        b.ctx().ns_per_byte = ns;
    }
    let total = if pacing_test {
        rng.range(10_000, 60_000)
    } else {
        // Fewer bytes when the chip sends them a few at a time (thousands of reads per block).
        let most = b.fake().policy.max_chunk.saturating_mul(1000).clamp(20_000, 200_000);
        rng.range(10_000, most)
    };
    let mut out = Vec::new();
    while out.len() < total {
        let len = match rng.below(10) {
            0 => 2 * block as usize,
            1 => block as usize + rng.range(1, 7),
            _ => block as usize,
        };
        let mut buf = vec![0u8; len];
        let rc = b.read_data(&mut buf);
        let want = len & !3;
        if rc != want as c_int {
            return Err(format!("read of {len} (block {block}) returned {rc}"));
        }
        out.extend_from_slice(&buf[..want]);
    }
    {
        let f = b.fake();
        if f.delivered != out.len() {
            return Err(format!(
                "the chip sent {} bytes, the driver handed on {}",
                f.delivered,
                out.len()
            ));
        }
        if let Some(i) = (0..out.len()).find(|&i| out[i] != f.produced[i]) {
            return Err(format!("byte {i} of the stream differs (block {block})"));
        }
    }
    if b.stop() != 0 {
        return Err("stop failed".into());
    }
    let h = b.close();
    let f = h.borrow();
    verdict(&f)?;
    Ok((out, f.log.clone().unwrap_or_default()))
}

#[test]
fn data_arrives_whole_and_in_order_in_any_chunking() {
    run_seeds(300, |seed| stream(seed, None, false).map(|_| ()));
}

// ---------------------------------------------------------------- 5. pacing

/// Pacing only sleeps: with it on (and a rate that makes it sleep at first) the driver sends
/// the chip exactly what it sends with it off, and hands on the same bytes. There is no fake
/// clock: the sleeps are real, and kept to a few milliseconds per seed.
#[test]
fn pacing_changes_nothing_but_when() {
    run_seeds(48, |seed| {
        let ns = 20.0 + Rng(seed ^ 0x9ACE).below(40) as f64;
        let (unpaced, log0) = stream(seed, None, true)?;
        let (paced, log1) = stream(seed, Some(ns), true)?;
        if unpaced != paced {
            return Err("paced and unpaced runs handed on different bytes".into());
        }
        if log0 != log1 {
            let i = (0..log0.len().min(log1.len()))
                .find(|&i| log0[i] != log1[i])
                .unwrap_or(0);
            return Err(format!(
                "paced and unpaced runs sent different things to the chip, from line {i}: {:?} vs {:?}",
                log0.get(i),
                log1.get(i)
            ));
        }
        Ok(())
    });
}

// ---------------------------------------------------------------- 4. faults

/// One whole session (open, register access, signal reads, streaming, a data write, stop,
/// more register access, close) with a stall, an unplug or a broken command endpoint at a
/// seeded point. Every call must return success or the error code its path has for a USB
/// failure; nothing may panic, spin or break the protocol; once the device is gone nothing
/// that needs it may succeed; and whatever succeeds must still be right.
fn faults(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let mut f = fake_for(seed, &mut rng);
    f.fault = match rng.below(4) {
        0 => Fault::Unplug { after: rng.below(400) },
        1 => Fault::Stall {
            ep: [0, 0x01, 0x02, 0x03, 0x82, 0x83][rng.below(6) as usize],
            from: rng.below(60),
        },
        2 => Fault::GoesSilent {
            after: rng.below(400),
            refuse_writes: rng.chance(500),
        },
        _ => Fault::ShortCommand { from: rng.below(80) },
    };
    let fault = f.fault;
    let (mut b, rc) = Board::open(f);
    let ctx = |what: &str, rc: c_int| format!("{fault:?}: {what} returned {rc}");
    if ![sys::ONI_ESUCCESS, sys::ONI_EINIT, sys::ONI_ESEEKFAILURE].contains(&rc) {
        return Err(ctx("init", rc));
    }
    let gone = |b: &Board| b.fake().gone;
    // Silent from before the call: nothing can have arrived for it.
    let silent = |b: &Board| matches!(fault, Fault::GoesSilent { after, .. } if b.fake().calls() >= after);
    let mut model: HashMap<(u32, u32), u32> = HashMap::new();
    let mut consumed = 0;
    // After a failed control read the parser may have lost bytes: later signal bytes cannot
    // be checked, so the signal stream is no longer read.
    let mut signal_ok = true;
    let mut reg_ops = |b: &mut Board, rng: &mut Rng, n: usize| -> Result<(), String> {
        for _ in 0..n {
            let (idx, addr) = (rng.below(2) as u32, rng.below(8) as u32);
            match rng.below(3) {
                0 => {
                    let v = value(rng);
                    match b.dev_write(idx, addr, v) {
                        0 if gone(b) => return Err(format!("{fault:?}: a register write succeeded after unplug")),
                        0 => {
                            model.insert((idx, addr), v);
                        }
                        // The write may or may not have reached the board.
                        sys::ONI_ESEEKFAILURE | sys::ONI_EINVALSTATE => {
                            model.remove(&(idx, addr));
                        }
                        rc => return Err(ctx("device register write", rc)),
                    }
                }
                1 => match (silent(b), b.dev_read(idx, addr)) {
                    (_, Ok(_)) if gone(b) => return Err(format!("{fault:?}: a register read succeeded after unplug")),
                    (true, Ok(_)) => return Err(format!("{fault:?}: a register read succeeded on a silent board")),
                    (_, Ok(v)) => {
                        if let Some(&want) = model.get(&(idx, addr))
                            && v != want
                        {
                            return Err(format!("{fault:?}: register read {v:#x}, not {want:#x}"));
                        }
                    }
                    (_, Err(sys::ONI_ESEEKFAILURE | sys::ONI_EINVALSTATE)) => {}
                    (_, Err(rc)) => return Err(ctx("device register read", rc)),
                },
                _ => {
                    let pending = b.fake().signal_sent.len() - consumed;
                    if pending == 0 || !signal_ok {
                        continue;
                    }
                    let n = rng.range(1, pending.min(512));
                    let mut buf = vec![0u8; n];
                    match b.read_signal(&mut buf) {
                        rc if rc == n as c_int => {
                            if buf != b.fake().signal_sent[consumed..consumed + n] {
                                return Err(format!("{fault:?}: signal stream differs at byte {consumed}"));
                            }
                            consumed += n;
                        }
                        sys::ONI_ESEEKFAILURE => signal_ok = false,
                        rc => return Err(ctx("signal read", rc)),
                    }
                }
            }
        }
        Ok(())
    };
    let n = rng.range(0, 20);
    reg_ops(&mut b, &mut rng, n)?;
    let block = 4 * rng.range(1, 2048) as u32;
    if b.start(block) != 0 {
        return Err("start failed".into());
    }
    let mut out = Vec::new();
    let mut stream_ok = true; // false once a read failed: the stream's position is lost
    for _ in 0..rng.range(0, 40) {
        if rng.chance(100) {
            let data: Vec<u8> = (0..rng.range(4, 64)).map(|_| rng.next() as u8).collect();
            match b.write_data(&data) {
                rc if rc == data.len() as c_int => {
                    if gone(&b) {
                        return Err(format!("{fault:?}: a data write succeeded after unplug"));
                    }
                }
                sys::ONI_EWRITEFAILURE | sys::ONI_EINVALSTATE => {}
                rc => return Err(ctx("data write", rc)),
            }
            continue;
        }
        let mut buf = vec![0u8; block as usize];
        let was_silent = silent(&b);
        match b.read_data(&mut buf) {
            rc if rc == block as c_int => {
                if gone(&b) {
                    return Err(format!("{fault:?}: a data read succeeded after unplug"));
                }
                if was_silent {
                    return Err(format!("{fault:?}: a data read succeeded on a silent board"));
                }
                if stream_ok {
                    out.extend_from_slice(&buf);
                }
            }
            sys::ONI_EREADFAILURE | sys::ONI_EINVALSTATE => stream_ok = false,
            rc => return Err(ctx("data read", rc)),
        }
    }
    if b.fake().produced[..out.len()] != out[..] {
        return Err(format!("{fault:?}: the data stream differs"));
    }
    if b.stop() != 0 {
        return Err("stop failed".into());
    }
    let n = rng.range(0, 10);
    reg_ops(&mut b, &mut rng, n)?;
    let h = b.close();
    verdict(&h.borrow()).map_err(|e| format!("{fault:?}: {e}"))
}

#[test]
fn stalls_unplugs_and_short_commands_give_errors_at_any_point() {
    run_seeds(400, faults);
}

#[test]
fn a_stalled_data_pipe_fails_the_read_and_the_next() {
    let mut f = Fake::new(1, Policy::exact());
    f.policy.produce_max = 4096;
    f.fault = Fault::Stall { ep: 0x82, from: 1 };
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.start(4096), 0);
    let mut buf = vec![0u8; 4096];
    assert_eq!(b.read_data(&mut buf), 4096);
    assert_eq!(b.read_data(&mut buf), sys::ONI_EREADFAILURE);
    assert_eq!(b.read_data(&mut buf), sys::ONI_EREADFAILURE);
    // The control pipe is unaffected.
    assert_eq!(b.dev_write(0, 1, 7), 0);
    assert_eq!(b.dev_read(0, 1), Ok(7));
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn a_stalled_control_pipe_fails_the_register_read() {
    let mut f = Fake::new(2, Policy::exact());
    f.fault = Fault::Stall { ep: 0x83, from: 0 };
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.dev_write(0, 1, 7), 0); // writes do not read the control pipe
    assert_eq!(b.read(sys::ONI_CONFIG_REG_VALUE), Err(sys::ONI_ESEEKFAILURE));
    let mut s = [0u8; 2];
    assert_eq!(b.read_signal(&mut s), sys::ONI_ESEEKFAILURE);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn unplugged_mid_read_the_data_read_fails() {
    for after in 0..30 {
        let mut f = Fake::new(3, Policy::exact());
        f.policy.produce_max = 1000; // short reads: a block takes several
        let (mut b, rc) = Board::open(f);
        assert_eq!(rc, 0);
        assert_eq!(b.start(4096), 0);
        let calls = b.fake().calls();
        b.fake().fault = Fault::Unplug { after: calls + after };
        let mut buf = vec![0u8; 4096];
        let mut failed = false;
        for _ in 0..10 {
            match b.read_data(&mut buf) {
                4096 => assert!(!b.fake().gone, "a read succeeded after unplug"),
                rc => {
                    assert_eq!(rc, sys::ONI_EREADFAILURE);
                    failed = true;
                }
            }
        }
        assert!(failed, "unplug after {after} more calls was never noticed");
        let h = b.close();
        verdict(&h.borrow()).unwrap();
    }
}

#[test]
fn a_signal_packet_that_never_ends_fails_the_register_read() {
    let mut f = Fake::new(4, Policy::exact());
    f.fault = Fault::Flood;
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    // The packet fills the 8 KiB signal buffer: an error, not a wait.
    assert_eq!(b.read(sys::ONI_CONFIG_RUNNING), Err(sys::ONI_ESEEKFAILURE));
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn a_short_command_fails_reads_and_writes() {
    let mut f = Fake::new(5, Policy::exact());
    f.policy.produce_max = 4096;
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.start(4096), 0);
    let from = b.fake().calls_on(0x01);
    b.fake().fault = Fault::ShortCommand { from };
    let mut buf = vec![0u8; 4096];
    assert_eq!(b.read_data(&mut buf), sys::ONI_EREADFAILURE);
    assert_eq!(b.write_data(&[0; 8]), sys::ONI_EWRITEFAILURE);
    assert_eq!(b.write(sys::ONI_CONFIG_RUNNING, 1), sys::ONI_ESEEKFAILURE);
    assert_eq!(b.read(sys::ONI_CONFIG_RUNNING), Err(sys::ONI_ESEEKFAILURE));
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn a_wrong_chip_configuration_is_refused_and_the_device_released() {
    for (what, change, want) in [
        ("short reply", 0usize, sys::ONI_EINIT),
        ("FIFO mode 245", 1, sys::ONI_EBADCONTROLLER),
        ("optional features on", 2, sys::ONI_EBADCONTROLLER),
    ] {
        let mut f = Fake::new(6, Policy::exact());
        match change {
            0 => f.config.truncate(100),
            1 => f.config[138] = 0,
            _ => f.config[140] = 0x10,
        }
        let (b, rc) = Board::open(f);
        assert_eq!(rc, want, "{what}");
        let h = b.close();
        verdict(&h.borrow()).unwrap_or_else(|e| panic!("{what}: {e}"));
    }
}

#[test]
fn a_busy_interface_is_reported_and_the_other_released() {
    let mut f = Fake::new(7, Policy::exact());
    f.fault = Fault::BusyInterface1;
    let (b, rc) = Board::open(f);
    assert_eq!(rc, sys::ONI_EINIT);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn a_second_board_that_is_not_there_is_edevidx() {
    let (b, rc) = Board::open_at(Fake::new(8, Policy::exact()), 1);
    assert_eq!(rc, sys::ONI_EDEVIDX);
    let h = b.close();
    assert!(h.borrow().log.is_none() && h.borrow().claimed == [false; 2]);
}

#[test]
fn a_failed_firmware_read_is_not_fatal() {
    let mut f = Fake::new(9, Policy::exact());
    f.firmware = None;
    let (b, rc) = Board::open(f);
    assert_eq!(rc, sys::ONI_ESUCCESS);
    let mut fw = 1u32;
    let mut len = 4usize;
    assert_eq!(get_opt(b.p, 0, (&mut fw as *mut u32).cast(), &mut len), 0);
    assert_eq!(fw, 0);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

// ---------------------------------------------------------------- 6. a board that stops answering

// Upstream (`onidriver_ft600.c`) retries forever, and so did this driver: on the board each
// retry is a 1000 ms (data, writes) or 100 ms (control) timeout, so the call never returned.
// Now each loop gives up once its stream has moved no byte for its limit (`DATA_SILENCE`,
// `CONTROL_SILENCE`, `WRITE_SILENCE`), on the fake's clock. The fake also ends any loop after
// `SPIN_LIMIT` empty transfers (`spun`).

/// What `f` returned, and how much of the fake's time it took.
fn timed<T>(b: &mut Board, f: impl FnOnce(&mut Board) -> T) -> (T, Duration) {
    let t0 = b.fake().now();
    let r = f(b);
    let dt = b.fake().now() - t0;
    (r, dt)
}

#[test]
fn a_silent_board_fails_the_data_read_instead_of_waiting_forever() {
    let mut f = Fake::new(10, Policy::exact());
    f.policy.produce_max = 4096;
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.start(4096), 0);
    b.fake().fault = Fault::Silent;
    let mut buf = vec![0u8; 4096];
    let (rc, dt) = timed(&mut b, |b| b.read_data(&mut buf));
    let spun = b.fake().spun;
    assert!(
        !spun,
        "read_data kept retrying (returned {rc} only when the fake gave up)"
    );
    assert_eq!(rc, sys::ONI_EREADFAILURE);
    // At the limit, within one more read's timeout.
    assert!(
        dt >= DATA_SILENCE && dt < DATA_SILENCE + Duration::from_secs(1),
        "gave up after {dt:?}"
    );
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn a_register_read_without_a_reply_fails_instead_of_waiting_forever() {
    let (mut b, rc) = Board::open(Fake::new(11, Policy::exact()));
    assert_eq!(rc, 0);
    b.fake().fault = Fault::Silent;
    let (r, dt) = timed(&mut b, |b| b.read(sys::ONI_CONFIG_RUNNING));
    let spun = b.fake().spun;
    assert!(
        !spun,
        "read_config kept retrying (returned {r:?} only when the fake gave up)"
    );
    assert_eq!(r, Err(sys::ONI_ESEEKFAILURE));
    assert!(
        dt >= CONTROL_SILENCE && dt < CONTROL_SILENCE + Duration::from_millis(100),
        "gave up after {dt:?}"
    );
    // The signal stream gives up the same way.
    let mut s = [0u8; 2];
    let (rc, dt) = timed(&mut b, |b| b.read_signal(&mut s));
    assert_eq!(rc, sys::ONI_ESEEKFAILURE);
    assert!(dt >= CONTROL_SILENCE && dt < CONTROL_SILENCE + Duration::from_millis(100));
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

#[test]
fn a_write_the_chip_never_accepts_fails_instead_of_retrying_forever() {
    let (mut b, rc) = Board::open(Fake::new(12, Policy::exact()));
    assert_eq!(rc, 0);
    b.fake().fault = Fault::OutRefuses;
    let (rc, dt) = timed(&mut b, |b| b.write(sys::ONI_CONFIG_RUNNING, 1));
    let spun = b.fake().spun;
    assert!(
        !spun,
        "send_cmd kept retrying (returned {rc} only when the fake gave up)"
    );
    assert_eq!(rc, sys::ONI_ESEEKFAILURE);
    assert!(
        dt >= WRITE_SILENCE && dt < WRITE_SILENCE + Duration::from_secs(1),
        "gave up after {dt:?}"
    );
    // A data frame write gives up the same way.
    let (rc, dt) = timed(&mut b, |b| b.write_data(&[0; 16]));
    assert_eq!(rc, sys::ONI_EWRITEFAILURE);
    assert!(dt >= WRITE_SILENCE && dt < WRITE_SILENCE + Duration::from_secs(1));
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

/// A board that is slow but never silent for the limit: each block takes far longer than
/// the limit, and no read gives up, because every byte starts the clock again.
#[test]
fn a_slow_but_moving_data_stream_never_gives_up() {
    let mut f = Fake::new(13, Policy::exact());
    f.policy.produce_max = 4; // 4 bytes before every read
    f.policy.timeout_empty = 1000; // every read times out empty...
    f.policy.max_quiet_ms = 2000; // ...unless the pipe has been quiet for 2 s: then it delivers
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.start(4096), 0);
    let mut out = Vec::new();
    for _ in 0..2 {
        let mut buf = vec![0u8; 4096];
        let (rc, dt) = timed(&mut b, |b| b.read_data(&mut buf));
        assert_eq!(rc, 4096);
        assert!(dt > 100 * DATA_SILENCE, "a block took only {dt:?}");
        out.extend_from_slice(&buf);
    }
    assert_eq!(out, b.fake().produced[..out.len()]);
    assert_eq!(b.stop(), 0);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

/// The same for writes: the chip takes a little at a time, with timeouts in between.
#[test]
fn a_slow_but_moving_write_never_gives_up() {
    let mut f = Fake::new(14, Policy::exact());
    f.policy.out_partial = 1000; // every write times out after part of the data, often none...
    f.policy.max_quiet_ms = 2000; // ...but never three times in a row
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.start(4096), 0);
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
    let (rc, dt) = timed(&mut b, |b| b.write_data(&data));
    assert_eq!(rc, data.len() as c_int);
    assert!(dt > WRITE_SILENCE, "the write took only {dt:?}");
    assert_eq!(b.fake().data_out, data);
    assert_eq!(b.write(sys::ONI_CONFIG_RUNNING, 1), 0);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

/// A register reply or signal packet after a silence shorter than the limit (a board slow
/// after a reset, say) is read as before.
#[test]
fn a_reply_after_a_silence_below_the_limit_is_read() {
    let (mut b, rc) = Board::open(Fake::new(15, Policy::exact()));
    assert_eq!(rc, 0);
    assert_eq!(b.write(sys::ONI_CONFIG_SYSCLKHZ, 0x1234_5678), 0);
    assert_eq!(b.write(sys::ONI_CONFIG_RESET, 1), 0);
    let just_below = CONTROL_SILENCE.as_millis() as u64 - 100;
    b.fake().hush[1] = just_below;
    let (r, dt) = timed(&mut b, |b| b.read(sys::ONI_CONFIG_SYSCLKHZ));
    assert_eq!(r, Ok(0x1234_5678));
    assert_eq!(dt, Duration::from_millis(just_below));
    // A device register write's acknowledgement on the signal stream.
    assert_eq!(b.dev_write(2, 5, 9), 0);
    b.fake().hush[1] = just_below;
    let mut s = [0u8; 3];
    assert_eq!(b.read_signal(&mut s), 3);
    assert_eq!(s, [0x57, 0x82, 0]);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

/// A reply later than the limit: the read fails, and the late reply is never taken for the
/// next read's (replies carry no tag), so register reads fail from then on, at once and
/// without sending anything.
#[test]
fn a_reply_later_than_the_limit_is_never_taken_for_the_next_one() {
    let (mut b, rc) = Board::open(Fake::new(16, Policy::exact()));
    assert_eq!(rc, 0);
    assert_eq!(b.write(sys::ONI_CONFIG_SYSCLKHZ, 111), 0);
    assert_eq!(b.write(sys::ONI_CONFIG_ACQCLKHZ, 222), 0);
    b.fake().hush[1] = CONTROL_SILENCE.as_millis() as u64 + 500;
    let (r, dt) = timed(&mut b, |b| b.read(sys::ONI_CONFIG_SYSCLKHZ));
    assert_eq!(r, Err(sys::ONI_ESEEKFAILURE));
    assert!(dt >= CONTROL_SILENCE);
    b.fake().log = Some(Vec::new());
    let (r, dt) = timed(&mut b, |b| b.read(sys::ONI_CONFIG_ACQCLKHZ));
    assert_eq!(r, Err(sys::ONI_ESEEKFAILURE));
    assert_eq!(dt, Duration::ZERO);
    assert_eq!(b.fake().log.take().unwrap(), Vec::<String>::new());
    // Writes still go out.
    assert_eq!(b.write(sys::ONI_CONFIG_RUNNING, 0), 0);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

/// Data is read only while acquiring: outside it a read is refused at once, sends nothing
/// and waits for nothing, as before the limit was added.
#[test]
fn a_data_read_outside_acquisition_is_refused_at_once() {
    let mut f = Fake::new(17, Policy::exact());
    f.policy.produce_max = 4096;
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.opt(sys::ONI_OPT_BLOCKREADSIZE, 4096), 0);
    let mut buf = vec![0u8; 4096];
    b.fake().log = Some(Vec::new());
    let (rc, dt) = timed(&mut b, |b| b.read_data(&mut buf));
    assert_eq!((rc, dt), (sys::ONI_EINVALSTATE, Duration::ZERO));
    assert_eq!(b.fake().log.take().unwrap(), Vec::<String>::new());
    assert_eq!(b.start(4096), 0);
    assert_eq!(b.read_data(&mut buf), 4096);
    assert_eq!(b.stop(), 0);
    b.fake().log = Some(Vec::new());
    let (rc, dt) = timed(&mut b, |b| b.read_data(&mut buf));
    assert_eq!((rc, dt), (sys::ONI_EINVALSTATE, Duration::ZERO));
    assert_eq!(b.fake().log.take().unwrap(), Vec::<String>::new());
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}

/// After giving up on a data read the chip's state is as after any timed-out read: when the
/// board speaks again the stream continues where it was, and stop and close abort the
/// pending announcement as usual (the protocol checks in `verdict`).
#[test]
fn after_giving_up_the_stream_continues_and_closes_cleanly() {
    let mut f = Fake::new(18, Policy::exact());
    f.policy.produce_max = 1000;
    let (mut b, rc) = Board::open(f);
    assert_eq!(rc, 0);
    assert_eq!(b.start(4096), 0);
    let mut buf = vec![0u8; 4096];
    assert_eq!(b.read_data(&mut buf), 4096);
    let mut out = buf.clone();
    b.fake().hush[0] = DATA_SILENCE.as_millis() as u64 + 1000;
    let mut part = vec![0u8; 4096];
    assert_eq!(b.read_data(&mut part), sys::ONI_EREADFAILURE);
    assert_eq!(b.read_data(&mut buf), 4096);
    out.extend_from_slice(&buf);
    // Nothing was lost on the wire: the second block is the stream's next 4096 bytes.
    assert_eq!(out, b.fake().produced[..8192]);
    assert_eq!(b.stop(), 0);
    let h = b.close();
    verdict(&h.borrow()).unwrap();
}
