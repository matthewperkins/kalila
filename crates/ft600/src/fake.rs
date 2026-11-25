//! A scripted FT600, with just enough of an ONI board behind it, for the driver's tests.
//!
//! While a fake is installed on a thread (`install`), `usb::find` reports its location and
//! `usb::Device::open` hands back a device whose four entry points (`claim_interface`,
//! `release_interface`, `control`, `bulk`) call the fake instead of the kernel. Everything
//! above `usbfs.rs` (`chip.rs`, `driver.rs`) is the production code. The fake sees exactly
//! what would have reached the kernel: a control request arrives here only after
//! `Ft600::control_transfer` has checked it against the allowlist, and the fake records a
//! violation if any other request arrives. The fake has no file descriptor and makes no system
//! call, so it is not a second path to a real transfer.
//!
//! The chip is modelled as the driver understands it (`chip.rs`): every IN transfer is
//! announced on EP 0x01 with its length; it ends when that many bytes have gone, or early with
//! a short packet when the chip's FIFO runs empty; a read the host gives up on keeps the bytes
//! that arrived. On top of that the policy scripts the awkward cases: reads that time out with
//! nothing, with part of the transfer (whole packets, the chip still owes the rest), just as a
//! short packet ended it, or just before a zero-length packet ended it on a packet boundary.
//! The model checks the protocol as it goes and records a violation (instead of panicking:
//! the driver's entry points catch panics) when the host announces a transfer while one is
//! still announced, writes without an announcement, or sends anything unexpected.
//!
//! Behind the chip: a data FIFO the board fills with seeded bytes before each data read, and
//! a control FIFO carrying register replies (to the ONI commands written on EP 0x03), signal
//! packets and noise bytes. Timeouts take no time.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::usb::{Location, Transfer, UsbError};

pub(crate) type Handle = Rc<RefCell<Fake>>;

thread_local! {
    static INSTALLED: RefCell<Option<Handle>> = const { RefCell::new(None) };
}

/// Makes `fake` the board that this thread's `usb::find` and `usb::Device::open` see.
pub(crate) fn install(fake: Fake) -> Handle {
    let h = Rc::new(RefCell::new(fake));
    INSTALLED.with(|i| *i.borrow_mut() = Some(h.clone()));
    h
}

pub(crate) fn uninstall() {
    INSTALLED.with(|i| *i.borrow_mut() = None);
}

/// `usb::find`: the installed fake's location, if there is one.
pub(crate) fn location() -> Option<Location> {
    INSTALLED.with(|i| i.borrow().as_ref().map(|f| f.borrow().loc))
}

/// `usb::Device::open`: the installed fake, if `loc` is its location.
pub(crate) fn attach(loc: Location) -> Option<Handle> {
    INSTALLED.with(|i| i.borrow().as_ref().filter(|f| f.borrow().loc == loc).cloned())
}

/// splitmix64, as in `engine/tests/simulation.rs`.
#[derive(Clone)]
pub(crate) struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// How the chip and board behave, per read. Rates are per mille.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    /// IN reads that give up with nothing (the board was slower than the timeout).
    pub timeout_empty: u64,
    /// IN reads that give up after whole packets; the chip still owes the rest.
    pub timeout_partial: u64,
    /// IN reads that give up just as a short packet ended the transfer.
    pub race_short: u64,
    /// IN reads that give up on a packet boundary just before a zero-length packet ended the
    /// transfer: the next read then completes at once with nothing.
    pub race_zlp: u64,
    /// The most bytes one IN transfer carries before the chip's FIFO runs empty.
    pub max_chunk: usize,
    /// The most bytes the board adds to the data FIFO before each data read (exactly that
    /// many with `produce_fixed`).
    pub produce_max: usize,
    pub produce_fixed: bool,
    /// Control reads before which the board sends a signal packet.
    pub signal: u64,
    /// Control reads before which noise bytes (neither packet marker) arrive.
    pub noise: u64,
    /// OUT transfers that time out after part of the data.
    pub out_partial: u64,
}

impl Policy {
    /// Every transfer completes whole; the board produces nothing unasked.
    pub fn exact() -> Policy {
        Policy {
            timeout_empty: 0,
            timeout_partial: 0,
            race_short: 0,
            race_zlp: 0,
            max_chunk: usize::MAX,
            produce_max: 0,
            produce_fixed: true,
            signal: 0,
            noise: 0,
            out_partial: 0,
        }
    }

    pub fn random(rng: &mut Rng) -> Policy {
        let rate = |rng: &mut Rng, max: u64| if rng.chance(300) { 0 } else { rng.below(max + 1) };
        Policy {
            timeout_empty: rate(rng, 300),
            timeout_partial: rate(rng, 200),
            race_short: rate(rng, 100),
            race_zlp: rate(rng, 100),
            max_chunk: match rng.below(4) {
                0 => rng.range(1, 16),
                1 => rng.range(1, 1100),
                2 => rng.range(1000, 9000),
                _ => usize::MAX,
            },
            produce_max: match rng.below(3) {
                0 => rng.range(1, 64),
                1 => rng.range(64, 4096),
                _ => rng.range(4096, 40000),
            },
            produce_fixed: false,
            signal: rate(rng, 400),
            noise: rate(rng, 200),
            out_partial: rate(rng, 200),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    /// The endpoint (0 = control requests) stalls from its `from`-th transfer (counting from
    /// 0) on. Nothing here clears a halt, so it stays stalled.
    Stall {
        ep: u8,
        from: u64,
    },
    /// The device goes away after `after` calls; every later call fails with `NoDevice`.
    Unplug {
        after: u64,
    },
    /// IN reads never deliver anything: a board that has gone quiet.
    Silent,
    /// OUT transfers on the data and control pipes accept nothing and time out.
    OutRefuses,
    /// EP 0x01 takes only 19 bytes of each command from its `from`-th on.
    ShortCommand {
        from: u64,
    },
    /// Another program holds interface 1.
    BusyInterface1,
    /// The board sends a signal packet that never ends (no 0 byte), 1 KiB per control read.
    Flood,
}

/// Consecutive transfers that moved nothing after which the host is taken to be spinning:
/// every later call fails, so whatever loop it is in ends, and `spun` says so.
const SPIN_LIMIT: u32 = 5000;

const SIGRES_SIGNAL: u8 = 1;
const SIGRES_REG: u8 = 2;

pub(crate) struct Fake {
    pub loc: Location,
    pub policy: Policy,
    pub fault: Fault,
    rng: Rng,
    data_rng: Rng,
    /// `None`: the firmware-version request stalls.
    pub firmware: Option<u32>,
    /// The chip-configuration reply.
    pub config: Vec<u8>,
    /// The transcript of everything the host did, while `Some`.
    pub log: Option<Vec<String>>,
    pub violations: Vec<String>,
    pub claimed: [bool; 2],
    /// GPIO levels and directions as last written (bits 0..1).
    pub gpio_level: u8,
    pub gpio_dir: u8,
    /// Bytes announced and not yet sent on 0x82 / 0x83, and whether a zero-length packet
    /// ending that pipe's transfer is still on its way.
    in_ann: [usize; 2],
    zlp: [bool; 2],
    /// Bytes announced and not yet received on 0x02 / 0x03.
    out_ann: [usize; 2],
    data_fifo: VecDeque<u8>,
    /// Every byte the board put into the data FIFO, in order.
    pub produced: Vec<u8>,
    /// How many of them the chip has sent the host (drains included).
    pub delivered: usize,
    ctl_fifo: VecDeque<u8>,
    flooding: bool,
    /// The signal stream as the driver should hand it on: each packet's bytes after its
    /// marker, up to and including its 0.
    pub signal_sent: Vec<u8>,
    ctl_out: Vec<u8>,
    /// The board's ONI configuration registers and device registers.
    pub cfg: [u32; 11],
    pub devregs: HashMap<(u32, u32), u32>,
    pub prio: Option<u32>,
    /// Bytes written on the data OUT pipe.
    pub data_out: Vec<u8>,
    calls: u64,
    ep_calls: HashMap<u8, u64>,
    idle: u32,
    /// True once the host went `SPIN_LIMIT` transfers without anything moving.
    pub spun: bool,
    pub gone: bool,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn done(actual: usize) -> Result<Transfer, UsbError> {
    Ok(Transfer {
        actual,
        timed_out: false,
    })
}

fn timeout(actual: usize) -> Result<Transfer, UsbError> {
    Ok(Transfer {
        actual,
        timed_out: true,
    })
}

fn outcome(r: &Result<Transfer, UsbError>) -> String {
    match r {
        Ok(t) => format!("{}{}", t.actual, if t.timed_out { " TIMEOUT" } else { "" }),
        Err(e) => format!("{e:?}"),
    }
}

impl Fake {
    /// A SuperSpeed FT600 in FIFO mode 600 with optional features off, firmware 1.05.
    pub fn new(seed: u64, policy: Policy) -> Fake {
        let mut config = vec![0u8; 152];
        config[0..2].copy_from_slice(&0x0403u16.to_le_bytes());
        config[2..4].copy_from_slice(&0x601eu16.to_le_bytes());
        config[137] = 0; // 100 MHz
        config[138] = 1; // FIFO mode 600
        config[139] = 0; // 4 channels
        Fake {
            loc: Location {
                bus: 9,
                dev: 99,
                speed: 5000,
            },
            policy,
            fault: Fault::None,
            rng: Rng(seed ^ 0xF7_600),
            data_rng: Rng(seed ^ 0xDA7A),
            firmware: Some(0x0105),
            config,
            log: None,
            violations: Vec::new(),
            claimed: [false; 2],
            gpio_level: 0,
            gpio_dir: 0,
            in_ann: [0; 2],
            zlp: [false; 2],
            out_ann: [0; 2],
            data_fifo: VecDeque::new(),
            produced: Vec::new(),
            delivered: 0,
            ctl_fifo: VecDeque::new(),
            flooding: false,
            signal_sent: Vec::new(),
            ctl_out: Vec::new(),
            cfg: [0; 11],
            devregs: HashMap::new(),
            prio: None,
            data_out: Vec::new(),
            calls: 0,
            ep_calls: HashMap::new(),
            idle: 0,
            spun: false,
            gone: false,
        }
    }

    /// USB calls so far (the count `Fault::Unplug` is against).
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// Transfers so far on an endpoint (the count `Fault::Stall` is against).
    pub fn calls_on(&self, ep: u8) -> u64 {
        self.ep_calls.get(&ep).copied().unwrap_or(0)
    }

    fn note(&mut self, s: impl FnOnce() -> String) {
        if let Some(log) = self.log.as_mut() {
            log.push(s());
        }
    }

    fn violation(&mut self, s: String) {
        self.note(|| format!("VIOLATION {s}"));
        self.violations.push(s);
    }

    /// Bookkeeping common to every transfer: faults that apply before anything moves.
    fn begin(&mut self, ep: u8) -> Result<(), UsbError> {
        self.calls += 1;
        if self.spun {
            return Err(UsbError::Io);
        }
        if let Fault::Unplug { after } = self.fault
            && self.calls > after
        {
            self.gone = true;
        }
        if self.gone {
            return Err(UsbError::NoDevice);
        }
        let n = self.ep_calls.entry(ep).or_insert(0);
        let k = *n;
        *n += 1;
        if let Fault::Stall { ep: s, from } = self.fault
            && s == ep
            && k >= from
        {
            return Err(UsbError::Stall);
        }
        Ok(())
    }

    fn moved(&mut self, n: usize) {
        if n > 0 {
            self.idle = 0;
        } else {
            self.idle += 1;
            if self.idle >= SPIN_LIMIT {
                self.spun = true;
                self.note(|| "SPIN: nothing moved for too long".into());
            }
        }
    }

    pub fn on_claim(&mut self, interface: u32) -> Result<(), UsbError> {
        self.note(|| format!("claim {interface}"));
        self.calls += 1;
        if self.gone {
            return Err(UsbError::NoDevice);
        }
        if interface > 1 {
            self.violation(format!("claimed interface {interface}"));
            return Err(UsbError::Io);
        }
        if interface == 1 && self.fault == Fault::BusyInterface1 {
            return Err(UsbError::Busy);
        }
        self.claimed[interface as usize] = true;
        Ok(())
    }

    pub fn on_release(&mut self, interface: u32) {
        self.note(|| format!("release {interface}"));
        match self.claimed.get_mut(interface as usize) {
            Some(c) if *c => *c = false,
            _ => self.violation(format!("released interface {interface}, not claimed")),
        }
    }

    pub fn on_control(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout_ms: u32,
    ) -> Result<usize, UsbError> {
        let what = if request_type & 0x80 != 0 {
            format!("in {}", data.len())
        } else {
            format!("out {}", hex(data))
        };
        let r = self.control_inner(request_type, request, value, index, data);
        let res = match &r {
            Ok(n) => n.to_string(),
            Err(e) => format!("{e:?}"),
        };
        self.note(|| {
            format!("control {request_type:02x} {request:02x} {value:04x} {index:04x} {what} t{timeout_ms} -> {res}")
        });
        r
    }

    fn control_inner(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> Result<usize, UsbError> {
        self.begin(0)?;
        if index != 0 {
            self.violation(format!("control request with index {index}"));
        }
        match (request_type, request) {
            (0xc0, 0xf1) => {
                let Some(fw) = self.firmware else {
                    return Err(UsbError::Stall);
                };
                let n = data.len().min(4);
                data[..n].copy_from_slice(&fw.to_le_bytes()[..n]);
                Ok(n)
            }
            (0xc0, 0xcf) if value == 1 => {
                let n = data.len().min(self.config.len());
                data[..n].copy_from_slice(&self.config[..n]);
                Ok(n)
            }
            (0x40, 0x02) if data.len() == 2 && (1..=3).contains(&value) => {
                let v = u16::from_le_bytes([data[0], data[1]]);
                let mask = ((v >> 2) & 3) as u8;
                match value {
                    1 => self.gpio_level = (self.gpio_level & !mask) | (v as u8 & mask),
                    2 => self.gpio_dir = (self.gpio_dir & !mask) | (v as u8 & mask),
                    _ => {}
                }
                Ok(2)
            }
            _ => {
                // Only requests that passed `Ft600::control_transfer`'s check can get here.
                self.violation(format!(
                    "control request {request_type:02x} {request:02x} value {value} len {} reached the device",
                    data.len()
                ));
                Err(UsbError::Stall)
            }
        }
    }

    pub fn on_bulk_out(&mut self, ep: u8, buf: &[u8], timeout_ms: u32) -> Result<Transfer, UsbError> {
        let r = self.bulk_out_inner(ep, buf);
        let res = outcome(&r);
        if ep == 0x01 && buf.len() == 20 {
            let len = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            let what = match buf[5] {
                1 => format!("transfer {:02x} {len}", buf[4]),
                3 => format!("abort {:02x}", buf[4]),
                c => format!("command {c}"),
            };
            self.note(|| format!("bulk 01 {} t{timeout_ms} -> {res}  # {what}", hex(buf)));
        } else {
            self.note(|| format!("bulk {ep:02x} {} t{timeout_ms} -> {res}", hex(buf)));
        }
        r
    }

    fn bulk_out_inner(&mut self, ep: u8, buf: &[u8]) -> Result<Transfer, UsbError> {
        self.begin(ep)?;
        match ep {
            0x01 => {
                if let Fault::ShortCommand { from } = self.fault
                    && self.ep_calls[&ep] > from
                {
                    return done(19);
                }
                self.command(buf);
                done(buf.len())
            }
            0x02 | 0x03 => {
                let i = (ep - 2) as usize;
                if self.out_ann[i] == 0 {
                    self.violation(format!(
                        "write of {} bytes on {ep:02x} without an announcement",
                        buf.len()
                    ));
                } else if buf.len() != self.out_ann[i] {
                    self.violation(format!(
                        "write of {} bytes on {ep:02x}, {} announced",
                        buf.len(),
                        self.out_ann[i]
                    ));
                }
                let n = if self.fault == Fault::OutRefuses {
                    0
                } else if !buf.is_empty() && self.rng.chance(self.policy.out_partial) {
                    self.rng.below(buf.len() as u64) as usize
                } else {
                    buf.len()
                };
                self.out_ann[i] = self.out_ann[i].saturating_sub(n);
                self.moved(n);
                if i == 0 {
                    self.data_out.extend_from_slice(&buf[..n]);
                } else {
                    self.ctl_out.extend_from_slice(&buf[..n]);
                    self.board_commands();
                }
                if n < buf.len() { timeout(n) } else { done(n) }
            }
            _ => {
                self.violation(format!("write on endpoint {ep:02x}"));
                Err(UsbError::Stall)
            }
        }
    }

    /// A 20-byte command on EP 0x01.
    fn command(&mut self, b: &[u8]) {
        if b.len() != 20 {
            self.violation(format!("command of {} bytes", b.len()));
            return;
        }
        if b[..4] != [0; 4] || b[6..8] != [0; 2] || b[12..] != [0; 8] {
            self.violation(format!("command with unexpected bytes {}", hex(b)));
        }
        let (pipe, cmd) = (b[4], b[5]);
        let len = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        match (cmd, pipe) {
            (1, 0x82 | 0x83) => {
                let i = (pipe - 0x82) as usize;
                if self.in_ann[i] != 0 {
                    self.violation(format!(
                        "transfer of {len} announced on {pipe:02x} while {} bytes are still announced",
                        self.in_ann[i]
                    ));
                }
                if len == 0 {
                    self.violation(format!("empty transfer announced on {pipe:02x}"));
                }
                self.in_ann[i] = len;
            }
            (1, 0x02 | 0x03) => self.out_ann[(pipe - 2) as usize] = len,
            (3, 0x82 | 0x83) => {
                let i = (pipe - 0x82) as usize;
                self.in_ann[i] = 0;
                self.zlp[i] = false;
            }
            (3, 0x02 | 0x03) => self.out_ann[(pipe - 2) as usize] = 0,
            _ => self.violation(format!("command {cmd} on pipe {pipe:02x}")),
        }
    }

    pub fn on_bulk_in(&mut self, ep: u8, buf: &mut [u8], timeout_ms: u32) -> Result<Transfer, UsbError> {
        let r = self.bulk_in_inner(ep, buf);
        let res = outcome(&r);
        let shown = match &r {
            Ok(t) if ep == 0x83 => format!(" {}", hex(&buf[..t.actual.min(24)])),
            _ => String::new(),
        };
        self.note(|| format!("bulk {ep:02x} [{}] t{timeout_ms} -> {res}{shown}", buf.len()));
        r
    }

    fn bulk_in_inner(&mut self, ep: u8, buf: &mut [u8]) -> Result<Transfer, UsbError> {
        self.begin(ep)?;
        if ep != 0x82 && ep != 0x83 {
            self.violation(format!("read on endpoint {ep:02x}"));
            return Err(UsbError::Stall);
        }
        let i = (ep - 0x82) as usize;
        if self.fault == Fault::Silent {
            self.moved(0);
            return timeout(0);
        }
        if self.zlp[i] {
            self.zlp[i] = false;
            self.moved(0);
            return done(0);
        }
        if self.in_ann[i] == 0 {
            // Nothing announced: the chip sends nothing.
            self.moved(0);
            return timeout(0);
        }
        if i == 0 {
            self.produce();
        } else {
            self.board_traffic();
        }
        let ps = self.loc.bulk_packet_size();
        let len = buf.len();
        let ann = self.in_ann[i];
        let cap = len.min(ann);
        let avail = if i == 0 {
            self.data_fifo.len()
        } else {
            self.ctl_fifo.len()
        };
        let p = self.policy;
        if self.rng.chance(p.timeout_empty) || cap == 0 {
            self.moved(0);
            return timeout(0);
        }
        // Each case below delivers `k` bytes and leaves the announcement as the chip has it.
        let roll = self.rng.below(1000);
        let below_cap = (cap - 1).min(avail); // what a transfer that did not complete can carry
        let (k, r) = if roll < p.timeout_partial && below_cap >= ps {
            let k = ps * self.rng.range(1, below_cap / ps);
            self.in_ann[i] -= k;
            (k, timeout(k))
        } else if roll < p.timeout_partial + p.race_short && below_cap >= 1 {
            let mut k = self.rng.range(1, below_cap);
            if k.is_multiple_of(ps) {
                k -= 1;
            }
            self.in_ann[i] = 0;
            (k, timeout(k))
        } else if roll < p.timeout_partial + p.race_short + p.race_zlp {
            let k = ps * self.rng.range(0, below_cap / ps);
            self.in_ann[i] = 0;
            self.zlp[i] = true;
            (k, timeout(k))
        } else {
            let chunk = if p.max_chunk == usize::MAX {
                usize::MAX
            } else {
                self.rng.range(1, p.max_chunk)
            };
            let k = cap.min(avail).min(chunk);
            if k == 0 {
                self.moved(0);
                return timeout(0);
            }
            if k == len {
                self.in_ann[i] -= k;
                (k, done(k))
            } else if k < ann {
                // The FIFO ran empty: a short (or zero-length) packet ends the transfer.
                self.in_ann[i] = 0;
                (k, done(k))
            } else {
                // The announcement is used up but the host asked for more: without a short
                // packet the host's transfer waits for its timeout.
                self.in_ann[i] = 0;
                (k, if k.is_multiple_of(ps) { timeout(k) } else { done(k) })
            }
        };
        let fifo = if i == 0 {
            &mut self.data_fifo
        } else {
            &mut self.ctl_fifo
        };
        for b in &mut buf[..k] {
            *b = fifo.pop_front().unwrap();
        }
        if i == 0 {
            self.delivered += k;
        }
        self.moved(k);
        r
    }

    /// The board adds samples to the data FIFO.
    fn produce(&mut self) {
        let p = self.policy;
        let n = if p.produce_fixed {
            p.produce_max
        } else {
            self.rng.range(0, p.produce_max)
        };
        for _ in 0..n {
            let b = self.data_rng.next() as u8;
            self.data_fifo.push_back(b);
            self.produced.push(b);
        }
    }

    /// The board sends unasked control traffic: signal packets, noise, or a flood.
    fn board_traffic(&mut self) {
        if self.fault == Fault::Flood {
            if !self.flooding {
                self.flooding = true;
                self.ctl_fifo.push_back(SIGRES_SIGNAL);
            }
            for _ in 0..1024 {
                self.ctl_fifo.push_back(1 + self.rng.below(255) as u8);
            }
            return;
        }
        if self.rng.chance(self.policy.noise) {
            for _ in 0..self.rng.range(1, 8) {
                // Any byte but the two packet markers, 0 included.
                let b = match self.rng.below(254) as u8 {
                    0 => 0,
                    b => b + 2,
                };
                self.ctl_fifo.push_back(b);
            }
        }
        if self.rng.chance(self.policy.signal) {
            let n = self.rng.range(1, 40);
            let payload: Vec<u8> = (0..n).map(|_| 1 + self.rng.below(255) as u8).collect();
            self.signal(&payload);
        }
    }

    fn signal(&mut self, payload: &[u8]) {
        self.ctl_fifo.push_back(SIGRES_SIGNAL);
        self.ctl_fifo.extend(payload);
        self.ctl_fifo.push_back(0);
        self.signal_sent.extend_from_slice(payload);
        self.signal_sent.push(0);
    }

    /// Runs the ONI commands written to the control pipe so far.
    fn board_commands(&mut self) {
        loop {
            let Some(&c) = self.ctl_out.first() else { return };
            let need = match c {
                1 => 9,
                2 | 3 => 5,
                _ => {
                    self.violation(format!("ONI command {c:02x} on the control pipe"));
                    self.ctl_out.clear();
                    return;
                }
            };
            if self.ctl_out.len() < need {
                return;
            }
            let b: Vec<u8> = self.ctl_out.drain(..need).collect();
            let a = u32::from_le_bytes(b[1..5].try_into().unwrap());
            match c {
                1 => self.write_reg(a, u32::from_le_bytes(b[5..9].try_into().unwrap())),
                // A flooding board is stuck in its packet: it answers nothing.
                _ if self.fault == Fault::Flood => {}
                2 => match self.cfg.get(a as usize) {
                    Some(&v) => {
                        self.ctl_fifo.push_back(SIGRES_REG);
                        self.ctl_fifo.extend(v.to_le_bytes());
                    }
                    None => self.violation(format!("read of ONI register {a}")),
                },
                _ => self.prio = Some(a),
            }
        }
    }

    fn write_reg(&mut self, reg: u32, value: u32) {
        let Some(r) = self.cfg.get_mut(reg as usize) else {
            self.violation(format!("write of ONI register {reg}"));
            return;
        };
        *r = value;
        if reg == 4 && value == 1 {
            // TRIG: the device register access latched in DEV_IDX, REG_ADDR, REG_VALUE, RW.
            let key = (self.cfg[0], self.cfg[1]);
            let rw = self.cfg[3] != 0;
            if rw {
                self.devregs.insert(key, self.cfg[2]);
            } else {
                self.cfg[2] = self
                    .devregs
                    .get(&key)
                    .copied()
                    .unwrap_or(key.1.wrapping_mul(0x9E37_79B9));
            }
            self.cfg[4] = 0;
            self.signal(&[if rw { 0x57 } else { 0x52 }, (key.0 as u8) | 0x80]);
        }
    }
}
