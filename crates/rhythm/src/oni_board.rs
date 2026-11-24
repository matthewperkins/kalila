//! Open Ephys acquisition board (ECP5 FPGA, FT600 USB3, ONI protocol).
//!
//! Port of `Rhd2000ONIBoard` + `AcqBoardONI` from the Open Ephys acquisition-board
//! plugin. Register writes, ordering and timing follow upstream; differences are
//! marked `DIFF:` and exist only to make failures visible instead of silent.

use crate::assemble::Assembled;
use crate::oni::{Context, Frame, OniResult};
use crate::parser::RhythmParser;
use crate::registers::Rhd2000Registers;
use crate::types::*;
use anyhow::{Context as _, Result, bail};
use log::{debug, info, warn};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

// ---- ONI device indices -------------------------------------------------------
pub const DEVICE_INFO: u32 = 254;
pub const DEVICE_RHYTHM: u32 = 0x0101;
pub const DEVICE_TTL: u32 = 0x0102;
pub const DEVICE_DAC: u32 = 0x0103;
pub const DEVICE_MEMORY: u32 = 0x0001;
pub const DEVICE_BNO_A: u32 = 0x0002;
pub const DEVICE_I2C_RAW_A: u32 = 0x0003;
const RHYTHM_HUB_MANAGER: u32 = 0x01FE;
const HUB_CLOCK_SEL: u32 = 0x2000;
const HUB_CLOCK_BUSY: u32 = 0x2001;

// ---- Rhythm registers ---------------------------------------------------------
mod reg {
    pub const MODE: u32 = 1;
    pub const MAX_TIMESTEP: u32 = 2;
    pub const CABLE_DELAY: u32 = 3;
    pub const AUXCMD_BANK_1: u32 = 4;
    pub const MAX_AUXCMD_INDEX_1: u32 = 7;
    pub const LOOP_AUXCMD_INDEX_1: u32 = 10;
    pub const DATA_STREAM_1_8_SEL: u32 = 13;
    pub const DATA_STREAM_EN: u32 = 15;
    pub const EXTERNAL_FAST_SETTLE: u32 = 16;
    pub const EXTERNAL_DIGOUT_A: u32 = 17;
    pub const SYNC_CLKOUT_DIVIDE: u32 = 21;
    pub const DAC_CTL: u32 = 22;
    pub const DAC_SEL_1: u32 = 23;
    pub const DAC_THRESH_1: u32 = 31;
    pub const HPF: u32 = 39;
    pub const SPI_RUNNING: u32 = 40;
}
const MODE_SPI_RUN_CONTINUOUS: u32 = 1;
const MODE_DSP_SETTLE: u32 = 2;
const MODE_TTL_OUT: u32 = 3;
const MODE_LED_ENABLE: u32 = 4;

const MAX_STREAMS: usize = 16;
const INIT_STEP: usize = 64;
const MEMORY_MONITOR_FS: u32 = 100;
use crate::parser::{ADC_BIT_VOLTS, RHD2000_MAGIC};

pub const CHIP_ID_RHD2132: i32 = 1;
pub const CHIP_ID_RHD2216: i32 = 2;
pub const CHIP_ID_RHD2164: i32 = 4;
pub use crate::parser::CHIP_ID_RHD2164_B;
const REGISTER_59_MISO_A: i32 = 53;
const DEVICE_ID_V3: u32 = 0x0102;
const BNO_CHANNELS: usize = 18;
/// Same scaling as the stock plugin (euler 1/16 deg, quaternion 2^-14, accel/gravity 0.01 m/s², temp/cal 1).
const BNO_BIT_VOLTS: [f32; BNO_CHANNELS] = [0.0625, 0.0625, 0.0625, 6.103_515_6e-5, 6.103_515_6e-5, 6.103_515_6e-5, 6.103_515_6e-5, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 1.0, 1.0, 1.0, 1.0, 1.0];

const HS_NAMES: [&str; 8] = ["A1", "A2", "B1", "B2", "C1", "C2", "D1", "D2"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuxSlot {
    Cmd1 = 0,
    Cmd2 = 1,
    Cmd3 = 2,
}

#[derive(Clone, Debug, Default)]
struct Headstage {
    num_streams: usize,
    channels_per_stream: usize,
    first_stream_index: usize,
    first_channel: usize,
    has_bno: bool,
    chip_id: i32,
}
impl Headstage {
    fn connected(&self) -> bool {
        self.num_streams > 0 || self.has_bno
    }
    fn num_channels(&self) -> usize {
        self.num_streams * self.channels_per_stream
    }
}

/// Supported per-channel rates and their hub clock settings (divEn, clockSel, divider).
const SAMPLE_RATES: [(u32, f64, u32, u32, u32); 15] = [
    (1000, 1000.0, 1, 2, 9),
    (1250, 1250.0, 1, 1, 9),
    (1500, 1500.0, 1, 0, 9),
    (2000, 2000.0, 1, 2, 4),
    (2500, 2500.0, 1, 1, 4),
    (3000, 3000.0, 1, 0, 4),
    (3333, 10000.0 / 3.0, 1, 2, 2),
    (5000, 5000.0, 1, 2, 1),
    (6250, 6250.0, 1, 1, 1),
    (10000, 10000.0, 1, 2, 0),
    (12500, 12500.0, 1, 1, 0),
    (15000, 15000.0, 1, 0, 0),
    (20000, 20000.0, 0, 2, 0),
    (25000, 25000.0, 0, 1, 0),
    (30000, 30000.0, 0, 0, 0),
];

/// Per-sample aux readout of the init/scan runs: aux[stream][slot][t].
struct ScanBlock {
    aux: Vec<[[u16; INIT_STEP]; 3]>,
}

/// How the FT600 is reached (`KALILA_DRIVER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    /// kalila's own USB code (`ft600` crate, ported from dimna): the default since
    /// 2026-11-24, after passing on the rig.
    Usb,
    /// liboni's `libonidriver_ft600.so` on FTDI's D3XX 1.0.x: `KALILA_DRIVER=d3xx`.
    D3xx,
}

impl Driver {
    pub fn from_env() -> Self {
        match std::env::var("KALILA_DRIVER") {
            Ok(v) if v.eq_ignore_ascii_case("d3xx") => Driver::D3xx,
            _ => Driver::Usb,
        }
    }
}

pub struct OniBoard {
    ctx: Context,
    settings: BoardSettings,
    regs: Rhd2000Registers,
    sample_rate: f64,
    stream_enabled: [bool; MAX_STREAMS],
    num_data_streams: usize,
    cable_delay: [i32; 4],
    optimum_delay: [i32; 4],
    headstages: [Headstage; 8],
    /// Data source (0..15) for each enabled stream, in stream order.
    enabled_streams: Vec<u32>,
    channels_per_stream: Vec<usize>,
    chip_ids: Vec<i32>,
    has_bno: [bool; 4],
    fw: (u32, u32, u32, u32),
    device_id: u32,
    has_i2c_support: bool,
    has_memmon: bool,
    total_memory: u32,
    acq_clock_hz: u32,
    common_commands_set: bool,
    initial_scan: bool,
    // ---- acquisition state ----
    health: Option<SharedHealth>,
    running: bool,
    pending: Option<Frame>,
    parser: RhythmParser,
    mem_sample: i64,
    bno_samples: [i64; 4],
    ttl_out: u16,
    dac_route: [Option<(usize, usize, f32)>; 8],
}

impl OniBoard {
    /// Open the first ONI acquisition board on the FT600 bus.
    ///
    /// The FT600 is reached through kalila's own USB code (the `ft600` crate), or with
    /// `KALILA_DRIVER=d3xx` through liboni's `libonidriver_ft600.so` on FTDI's D3XX.
    pub fn open() -> Result<Self> {
        let d3xx = Driver::from_env() == Driver::D3xx;
        let ctx = if d3xx {
            info!("Searching for ONI acquisition board (driver ft600 on D3XX, KALILA_DRIVER=d3xx)...");
            match crate::oni::configure_driver_search() {
                Some(d) => info!("using libonidriver_ft600.so from {}", d.display()),
                None => info!("libonidriver_ft600.so not found in known locations; trying the default library path"),
            }
            Context::open("ft600", -1)?
        } else {
            info!("Searching for ONI acquisition board (kalila's own FT600 driver, no D3XX)...");
            ft600::register();
            Context::open(ft600::DRIVER_NAME, -1)?
        };
        let mut b = Self {
            ctx,
            settings: BoardSettings::default(),
            regs: Rhd2000Registers::new(30000.0),
            sample_rate: 30000.0,
            stream_enabled: [false; MAX_STREAMS],
            num_data_streams: 0,
            cable_delay: [-1; 4],
            optimum_delay: [-1; 4],
            headstages: Default::default(),
            enabled_streams: Vec::new(),
            channels_per_stream: Vec::new(),
            chip_ids: Vec::new(),
            has_bno: [false; 4],
            fw: (0, 0, 0, 0),
            device_id: 0,
            has_i2c_support: false,
            has_memmon: false,
            total_memory: 0,
            acq_clock_hz: 0,
            common_commands_set: false,
            initial_scan: true,
            health: None,
            running: false,
            pending: None,
            parser: RhythmParser::default(),
            mem_sample: 0,
            bno_samples: [0; 4],
            ttl_out: 0,
            dac_route: [None; 8],
        };
        b.detect()?;
        b.initialize()?;
        b.scan_ports()?;
        Ok(b)
    }

    fn detect(&mut self) -> Result<()> {
        let (a, b, c) = Context::library_version();
        info!("liboni {a}.{b}.{c}");
        if let Some(d) = self.ctx.driver_info() {
            info!("ONI driver {} {}.{}.{}{}", d.name, d.version.0, d.version.1, d.version.2, d.pre_release.map(|p| format!("-{p}")).unwrap_or_default());
        }
        // Driver option 0 is the D3XX driver version on liboni's ft600 driver; kalila's own
        // driver logs the FT60x firmware itself when it opens the chip.
        if Driver::from_env() == Driver::D3xx {
            if let Ok(v) = self.ctx.get_driver_opt_u32(0) {
                info!("FTDI driver {}.{}.{}", (v >> 24) & 0xff, (v >> 16) & 0xff, v & 0xffff);
            }
            if let Ok(v) = self.ctx.get_driver_opt_u32(1) {
                info!("FTDI library {}.{}.{}", (v >> 24) & 0xff, (v >> 16) & 0xff, v & 0xff);
            }
        }
        let v = self.ctx.read_reg(DEVICE_INFO, 2).context("reading gateware version")?;
        self.fw = ((v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff, (v >> 24) & 0xff);
        let (ma, mi, pa, _) = self.fw;
        info!("ECP5-ONI gateware v{ma}.{mi}.{pa}");
        if ma == 0 {
            warn!("Gateware v{ma}.{mi}.{pa} is outdated and should be updated.");
        }
        let semver_ge = |t: (u32, u32, u32)| (ma, mi, pa) >= t;
        self.has_i2c_support = semver_ge((1, 5, 0));
        self.has_memmon = semver_ge((1, 5, 1));
        self.device_id = self.ctx.read_reg(DEVICE_INFO, 0).unwrap_or(0);
        info!("Acquisition board ID = {:#x}", self.device_id);
        if self.has_memmon {
            self.ctx.write_reg(DEVICE_MEMORY, 0, 1)?;
            let clk = self.ctx.read_reg(DEVICE_MEMORY, 2)?;
            self.ctx.write_reg(DEVICE_MEMORY, 1, clk / MEMORY_MONITOR_FS)?;
            self.total_memory = self.ctx.read_reg(DEVICE_MEMORY, 3)?;
        } else {
            let _ = self.ctx.write_reg(DEVICE_MEMORY, 0, 0);
        }
        if let Ok(t) = self.ctx.device_table() {
            for d in t {
                debug!("dev idx {:#x} id {:#x} ver {:#x} rd {} wr {}", d.idx, d.id, d.version, d.read_size, d.write_size);
            }
        }
        Ok(())
    }

    fn check_board_mem(&self) -> Result<()> {
        let v = self.ctx.read_reg(DEVICE_INFO, 0x1000)? & 3;
        match v {
            1 => Ok(()),
            0 => bail!("board memory still initializing; wait and retry"),
            3 => bail!("on-board memory error: power-cycle the board (USB and power)"),
            _ => bail!("invalid board memory state"),
        }
    }

    // ---------------------------------------------------------------- low level
    fn reset_board(&self) -> OniResult<()> {
        std::thread::sleep(Duration::from_millis(50));
        self.ctx.set_opt_u32(oni_sys::ONI_OPT_RESET, 1)?;
        self.ctx.set_opt_u32(oni_sys::ONI_OPT_BLOCKREADSIZE, self.settings.usb_block_bytes)
    }
    fn rw(&self, addr: u32, val: u32, mask: u32) -> OniResult<()> {
        self.ctx.write_reg_mask(DEVICE_RHYTHM, addr, val, mask)
    }
    fn set_continuous(&self, on: bool) -> OniResult<()> {
        self.rw(reg::MODE, (on as u32) << MODE_SPI_RUN_CONTINUOUS, 1 << MODE_SPI_RUN_CONTINUOUS)
    }
    fn set_max_timestep(&self, n: u32) -> OniResult<()> {
        self.ctx.write_reg(DEVICE_RHYTHM, reg::MAX_TIMESTEP, n)
    }
    fn run(&self) -> OniResult<()> {
        self.ctx.set_opt_u32(oni_sys::ONI_OPT_RESETACQCOUNTER, 2)
    }
    fn stop_hw(&self) -> OniResult<()> {
        self.ctx.set_opt_u32(oni_sys::ONI_OPT_RUNNING, 0)
    }
    fn is_running(&self) -> bool {
        self.ctx.read_reg(DEVICE_RHYTHM, reg::SPI_RUNNING).map(|v| v != 0).unwrap_or(false)
    }
    fn set_cable_delay(&mut self, port: usize, delay: i32) -> OniResult<()> {
        if !(0..=15).contains(&delay) {
            warn!("cable delay out of range: {delay}");
        }
        let d = delay.clamp(0, 15);
        self.cable_delay[port] = d;
        let sh = 4 * port as u32;
        self.rw(reg::CABLE_DELAY, (d as u32) << sh, 0xf << sh)
    }
    fn set_cable_length_m(&mut self, port: usize, m: f64) -> OniResult<()> {
        let t_step = 1.0 / (2800.0 * self.sample_rate);
        let v = 0.555 * 299_792_458.0;
        let td = 2.0 * m / v + 1.9e-9 + 9.0e-9 + 1.4e-9 + 6.7e-9;
        let mut d = ((td / t_step) + 1.0 + 0.5).floor() as i32;
        if d < 1 {
            d = 1;
        }
        self.set_cable_delay(port, d + 1)
    }
    fn upload_command_list(&self, list: &[i32], slot: AuxSlot, bank: u32) -> OniResult<()> {
        let base = 0x4000u32 + (bank << 10) + ((slot as u32) << 14);
        for (i, &c) in list.iter().enumerate() {
            self.ctx.write_reg(DEVICE_RHYTHM, base + i as u32, c as u32)?;
        }
        Ok(())
    }
    fn select_aux_bank(&self, port: usize, slot: AuxSlot, bank: u32) -> OniResult<()> {
        let sh = 4 * port as u32;
        self.rw(reg::AUXCMD_BANK_1 + slot as u32, bank << sh, 0xf << sh)
    }
    fn select_aux_bank_all(&self, slot: AuxSlot, bank: u32) -> OniResult<()> {
        (0..4).try_for_each(|p| self.select_aux_bank(p, slot, bank))
    }
    fn select_aux_length(&self, slot: AuxSlot, loop_idx: u32, end_idx: u32) -> OniResult<()> {
        self.ctx.write_reg(DEVICE_RHYTHM, reg::LOOP_AUXCMD_INDEX_1 + slot as u32, loop_idx)?;
        self.ctx.write_reg(DEVICE_RHYTHM, reg::MAX_AUXCMD_INDEX_1 + slot as u32, end_idx)
    }
    fn set_dsp_settle(&self, on: bool) -> OniResult<()> {
        self.rw(reg::MODE, (on as u32) << MODE_DSP_SETTLE, 1 << MODE_DSP_SETTLE)
    }
    fn set_data_source(&self, stream: usize, src: u32) -> OniResult<()> {
        let sh = 4 * (stream % 8) as u32;
        self.rw(reg::DATA_STREAM_1_8_SEL + (stream / 8) as u32, src << sh, 0xf << sh)
    }
    fn enable_data_stream(&mut self, stream: usize, on: bool) -> OniResult<()> {
        if self.stream_enabled[stream] != on {
            self.rw(reg::DATA_STREAM_EN, (on as u32) << stream, 1 << stream)?;
            self.stream_enabled[stream] = on;
            if on {
                self.num_data_streams += 1
            } else {
                self.num_data_streams -= 1
            }
        }
        Ok(())
    }
    fn enable_dac(&self, ch: usize, on: bool) -> OniResult<()> {
        self.rw(reg::DAC_SEL_1 + ch as u32, if on { 0x400 } else { 0 }, 0x400)
    }
    fn select_dac_stream(&self, ch: usize, stream: u32) -> OniResult<()> {
        self.rw(reg::DAC_SEL_1 + ch as u32, stream << 5, 0x1f << 5)
    }
    fn select_dac_channel(&self, ch: usize, chan: u32) -> OniResult<()> {
        self.rw(reg::DAC_SEL_1 + ch as u32, chan, 0x1f)
    }
    fn set_dac_gain(&self, g: u32) -> OniResult<()> {
        self.rw(reg::DAC_CTL, g << 7, 0x7 << 7)
    }
    fn set_audio_noise_suppress(&self, n: i32) -> OniResult<()> {
        self.rw(reg::DAC_CTL, n.clamp(0, 127) as u32, 0x7f)
    }
    fn set_dac_threshold(&self, ch: usize, thr: i32, pol: bool) -> OniResult<()> {
        let v = (thr.clamp(0, 65535) as u32 & 0xffff) | ((pol as u32) << 16);
        self.ctx.write_reg(DEVICE_RHYTHM, reg::DAC_THRESH_1 + ch as u32, v)
    }
    fn set_ttl_mode(&self, m: u32) -> OniResult<()> {
        self.rw(reg::MODE, m << MODE_TTL_OUT, 1 << MODE_TTL_OUT)
    }
    fn enable_external_fast_settle(&self, on: bool) -> OniResult<()> {
        self.rw(reg::EXTERNAL_FAST_SETTLE, (on as u32) << 4, 1 << 4)
    }
    fn set_external_fast_settle_channel(&self, ch: u32) -> OniResult<()> {
        self.rw(reg::EXTERNAL_FAST_SETTLE, ch & 0xf, 0xf)
    }
    fn enable_external_digout(&self, port: usize, on: bool) -> OniResult<()> {
        self.rw(reg::EXTERNAL_DIGOUT_A + port as u32, (on as u32) << 4, 1 << 4)
    }
    fn set_external_digout_channel(&self, port: usize, ch: u32) -> OniResult<()> {
        self.rw(reg::EXTERNAL_DIGOUT_A + port as u32, ch & 0xf, 0xf)
    }
    fn enable_dac_hpf(&self, on: bool) -> OniResult<()> {
        self.rw(reg::HPF, (on as u32) << 16, 1 << 16)
    }
    fn set_dac_hpf(&self, hz: f64) -> OniResult<()> {
        let b = 1.0 - (-2.0 * std::f64::consts::PI * hz / self.sample_rate).exp();
        let c = ((65536.0 * b + 0.5).floor() as i32).clamp(1, 65535);
        self.rw(reg::HPF, c as u32, 0xffff)
    }
    fn enable_leds(&self, on: bool) -> OniResult<()> {
        self.rw(reg::MODE, (on as u32) << MODE_LED_ENABLE, 1 << MODE_LED_ENABLE)
    }
    fn set_clock_divider(&self, ratio: u32) -> OniResult<()> {
        let r = if ratio != 1 && ratio % 2 == 1 { ratio - 1 } else { ratio };
        let f = if r <= 1 { 0 } else { r / 2 };
        self.ctx.write_reg(DEVICE_RHYTHM, reg::SYNC_CLKOUT_DIVIDE, f)
    }
    fn write_ttl_out(&self, word: u16) -> OniResult<()> {
        let mut b = (word as u32).to_le_bytes();
        self.ctx.write_frame(DEVICE_TTL, &mut b)
    }
    fn set_dac_manual(&self, value: i32) -> OniResult<()> {
        // Faithful to upstream's conversion (which saturates for any midrange value).
        let bit16 = 1.0 / 65536.0;
        let v = (((value & 0xffff) as f64 + 5.0) / (10.0 * bit16)).min(u16::MAX as f64) as u32;
        let mut buf = [0u8; 16];
        for i in 0..4 {
            buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        self.ctx.write_frame(DEVICE_DAC, &mut buf)
    }

    fn set_sample_rate_hw(&mut self, hz: u32) -> Result<()> {
        let &(_, actual, div_en, sel, div) = SAMPLE_RATES.iter().find(|r| r.0 == hz).with_context(|| format!("unsupported sample rate {hz}"))?;
        let v = ((div & 0xf) << 3) + ((sel & 3) << 1) + (div_en & 1);
        self.ctx.write_reg(RHYTHM_HUB_MANAGER, HUB_CLOCK_SEL, v)?;
        // DIFF: upstream polls forever; we time out so a wedged board is reported.
        let t0 = Instant::now();
        while self.ctx.read_reg(RHYTHM_HUB_MANAGER, HUB_CLOCK_BUSY)? != 0 {
            if t0.elapsed() > Duration::from_secs(2) {
                bail!("timeout waiting for sample-clock PLL");
            }
        }
        self.sample_rate = actual;
        Ok(())
    }

    // --------------------------------------------------------------- high level
    /// `Rhd2000ONIBoard::initialize` followed by `AcqBoardONI::initializeBoardInThread`.
    fn initialize(&mut self) -> Result<()> {
        info!("Initializing ONI acquisition board");
        self.reset_board()?;
        self.set_sample_rate_hw(30000)?;
        for slot in [AuxSlot::Cmd1, AuxSlot::Cmd2, AuxSlot::Cmd3] {
            self.select_aux_bank_all(slot, 0)?;
            self.select_aux_length(slot, 0, 0)?;
        }
        self.set_continuous(true)?;
        self.set_max_timestep(u32::MAX)?;
        for p in 0..4 {
            self.set_cable_length_m(p, 3.0 * 0.3048)?;
        }
        self.set_dsp_settle(false)?;
        let srcs = [0, 2, 4, 6, 1, 3, 5, 7];
        for i in 0..16 {
            self.set_data_source(i, srcs[i % 8])?;
        }
        self.enable_data_stream(0, true)?;
        for i in 1..MAX_STREAMS {
            self.enable_data_stream(i, false)?;
        }
        self.reset_board()?;
        self.write_ttl_out(0)?;
        for d in 0..8 {
            self.enable_dac(d, false)?;
            self.select_dac_stream(d, 0)?;
            self.select_dac_channel(d, 0)?;
        }
        self.set_dac_manual(32768)?;
        self.set_dac_gain(0)?;
        self.set_audio_noise_suppress(0)?;
        self.set_ttl_mode(1)?;
        for d in 0..8 {
            self.set_dac_threshold(d, 32768, true)?;
        }
        self.enable_external_fast_settle(false)?;
        self.set_external_fast_settle_channel(0)?;
        for p in 0..4 {
            self.enable_external_digout(p, false)?;
            self.set_external_digout_channel(p, 0)?;
        }
        self.enable_leds(true)?;

        // initializeBoardInThread
        self.set_sample_rate(30000, false)?;
        for p in 0..4 {
            let m = self.settings.cable_length_m[p] as f64;
            self.set_cable_length_m(p, m)?;
        }
        self.select_aux_bank_all(AuxSlot::Cmd3, 0)?;
        self.set_max_timestep(INIT_STEP as u32)?;
        self.set_continuous(false)?;
        self.run()?;
        let t0 = Instant::now();
        while self.is_running() {
            if t0.elapsed() > Duration::from_secs(2) {
                warn!("ADC calibration run did not finish within 2 s");
                break;
            }
        }
        self.stop_hw()?;
        let bank = if self.settings.fast_settle { 2 } else { 1 };
        self.select_aux_bank_all(AuxSlot::Cmd3, bank)?;
        self.acq_clock_hz = self.ctx.get_opt_u32(oni_sys::ONI_OPT_ACQCLKHZ)?;
        info!("Acquisition clock {} Hz", self.acq_clock_hz);
        Ok(())
    }

    fn set_sample_rate(&mut self, hz: u32, rescan: bool) -> Result<()> {
        self.set_sample_rate_hw(hz)?;
        self.settings.sample_rate = hz;
        info!("Sample rate set to {} Hz", self.sample_rate);
        if rescan {
            self.check_all_cable_delays()?;
        }
        self.update_registers()
    }

    fn update_registers(&mut self) -> Result<()> {
        self.regs.define_sample_rate(self.sample_rate);
        if !self.common_commands_set {
            self.regs.set_dig_out_low();
            let l = self.regs.command_list_update_dig_out();
            self.upload_command_list(&l, AuxSlot::Cmd1, 0)?;
            self.select_aux_length(AuxSlot::Cmd1, 0, l.len() as u32 - 1)?;
            self.select_aux_bank_all(AuxSlot::Cmd1, 0)?;
            let l = self.regs.command_list_temp_sensor();
            self.upload_command_list(&l, AuxSlot::Cmd2, 0)?;
            self.select_aux_length(AuxSlot::Cmd2, 0, l.len() as u32 - 1)?;
            self.select_aux_bank_all(AuxSlot::Cmd2, 0)?;
            self.common_commands_set = true;
        }
        let s = &mut self.settings;
        s.dsp_cutoff = self.regs.set_dsp_cutoff_freq(s.dsp_cutoff);
        let lo = self.regs.set_lower_bandwidth(s.lower_bandwidth);
        let up = self.regs.set_upper_bandwidth(s.upper_bandwidth);
        debug!("bandwidth requested {}..{} Hz, actual {lo:.3}..{up:.1} Hz", s.lower_bandwidth, s.upper_bandwidth);
        self.regs.enable_dsp(s.dsp_enabled);
        self.regs.enable_aux(s.acquire_aux);
        let fast = s.fast_settle;

        let l = self.regs.command_list_register_config(true);
        self.upload_command_list(&l, AuxSlot::Cmd3, 0)?;
        self.select_aux_length(AuxSlot::Cmd3, 0, l.len() as u32 - 1)?;
        let l = self.regs.command_list_register_config(false);
        self.upload_command_list(&l, AuxSlot::Cmd3, 1)?;
        self.select_aux_length(AuxSlot::Cmd3, 0, l.len() as u32 - 1)?;
        self.regs.set_fast_settle(true);
        let l = self.regs.command_list_register_config(false);
        self.upload_command_list(&l, AuxSlot::Cmd3, 2)?;
        self.select_aux_length(AuxSlot::Cmd3, 0, l.len() as u32 - 1)?;
        self.regs.set_fast_settle(false);
        self.select_aux_bank_all(AuxSlot::Cmd3, if fast { 2 } else { 1 })?;
        Ok(())
    }

    /// Run `n` frames of the (already started) SPI sequence and collect aux words.
    fn read_scan_block(&self, n: usize) -> Result<ScanBlock> {
        let ns = self.num_data_streams;
        let mut sb = ScanBlock { aux: vec![[[0u16; INIT_STEP]; 3]; ns] };
        let mut t = 0;
        // DIFF: upstream waits for as long as it takes; a board that sends no Rhythm frames
        // would leave the scan (and the board) busy for good. n frames take n / 30 kHz.
        let deadline = Instant::now() + Duration::from_secs(2);
        while t < n {
            if Instant::now() > deadline {
                bail!("port scan: only {t} of {n} headstage frames arrived within 2 s");
            }
            let f = self.ctx.read_frame()?;
            if f.dev_idx() != DEVICE_RHYTHM {
                continue;
            }
            let d = f.data().get(8..).unwrap_or(&[]);
            if d.len() < 12 + 6 * ns || u64::from_le_bytes(d[0..8].try_into().unwrap()) != RHD2000_MAGIC {
                warn!("incorrect Rhythm header during scan");
                t += 1;
                continue;
            }
            for slot in 0..3 {
                for s in 0..ns {
                    let o = 12 + 2 * (slot * ns + s);
                    sb.aux[s][slot][t] = u16::from_le_bytes([d[o], d[o + 1]]);
                }
            }
            t += 1;
        }
        Ok(sb)
    }

    /// Returns (chip id, register 59) or (-1,-1) when SPI communication is bad.
    fn intan_chip_id(sb: &ScanBlock, stream: usize) -> (i32, i32) {
        let a = &sb.aux[stream][2];
        let ch = |i: usize| a[i] as u8 as char;
        let ok = ch(32) == 'I' && ch(33) == 'N' && ch(34) == 'T' && ch(35) == 'A' && ch(36) == 'N' && ch(24) == 'R' && ch(25) == 'H' && ch(26) == 'D';
        if !ok { (-1, -1) } else { (a[19] as i32, a[23] as i32) }
    }

    fn good_chip(id: i32, r59: i32) -> bool {
        id == CHIP_ID_RHD2132 || id == CHIP_ID_RHD2216 || (id == CHIP_ID_RHD2164 && r59 == REGISTER_59_MISO_A)
    }

    fn delay_for_port(&self, port: usize, adjust: bool) -> i32 {
        let mut d = self.optimum_delay[port];
        if adjust {
            d += self.settings.cable_delay_adjust[port].clamp(0, 1);
        }
        d.clamp(0, 15)
    }

    fn check_all_cable_delays(&mut self) -> Result<()> {
        let connected: Vec<bool> = (0..4).map(|p| self.headstages[2 * p].connected() || self.headstages[2 * p + 1].connected()).collect();
        self.select_aux_bank_all(AuxSlot::Cmd3, 0)?;
        self.set_max_timestep(128 * INIT_STEP as u32)?;
        self.set_continuous(false)?;
        let mut sum = [0; 8];
        let mut first = [-1; 8];
        let mut second = [-1; 8];
        for delay in -2..2 {
            for p in 0..4 {
                let d = (self.delay_for_port(p, false) + delay).clamp(0, 15);
                self.set_cable_delay(p, d)?;
            }
            self.run()?;
            let sb = self.read_scan_block(INIT_STEP);
            self.stop_hw()?;
            let sb = sb?;
            for hs in 0..8 {
                let h = &self.headstages[hs];
                if h.num_streams > 0 {
                    let (id, r59) = Self::intan_chip_id(&sb, h.first_stream_index);
                    if Self::good_chip(id, r59) {
                        sum[hs] += 1;
                        if first[hs] == -1 {
                            first[hs] = delay;
                        } else if second[hs] == -1 {
                            second[hs] = delay;
                        }
                    }
                }
            }
        }
        let mut opt = [-5; 8];
        for hs in 0..8 {
            if sum[hs] == 1 || sum[hs] == 2 {
                opt[hs] = first[hs];
            } else if sum[hs] > 2 {
                opt[hs] = second[hs];
            }
        }
        for p in 0..4 {
            if connected[p] {
                let shift = opt[2 * p].max(opt[2 * p + 1]);
                let d = (self.delay_for_port(p, true) + shift).clamp(0, 15);
                self.set_cable_delay(p, d)?;
                debug!("port {} cable delay at {} Hz: {d}", "ABCD".as_bytes()[p] as char, self.sample_rate);
            }
        }
        Ok(())
    }

    fn i2c_read_byte(&self, dev: u32, i2c_addr: u32, reg_addr: u32) -> OniResult<u32> {
        let a = ((reg_addr << 7) | (i2c_addr & 0x7f)) | 0x8000_0000;
        self.ctx.read_reg(dev, a)
    }
    fn eeprom_device_id(&self, port: u32) -> u32 {
        let dev = DEVICE_I2C_RAW_A + port * 2;
        let rd = |a: u32| self.i2c_read_byte(dev, 0x50, a).map(|v| (v & 0xff) as u8);
        for (i, c) in b"OESH".iter().enumerate() {
            if rd(i as u32).ok() != Some(*c) {
                return 0;
            }
        }
        match rd(4) {
            Ok(1) | Ok(2) => {}
            _ => return 0,
        }
        let mut id = 0u32;
        for i in 0..4 {
            match rd(8 + i) {
                Ok(b) => id += (b as u32) << (8 * i),
                Err(_) => return 0,
            }
        }
        id
    }
    fn bno_connected(&self, port: u32) -> bool {
        let dev = DEVICE_BNO_A + port * 2;
        let t0 = Instant::now();
        let mut v = 2;
        while v >= 2 && t0.elapsed() < Duration::from_secs(1) {
            match self.ctx.read_reg(dev, 2) {
                Ok(x) => v = x,
                Err(_) => return false,
            }
        }
        v == 1
    }

    fn enable_headstage(&mut self, hs: usize, enabled: bool, nstr: usize, chans: usize, bno: bool, chip: i32) {
        let first_channel: usize = self.channels_per_stream.iter().sum();
        let h = &mut self.headstages[hs];
        if enabled {
            h.first_channel = first_channel;
            h.num_streams = nstr;
            h.channels_per_stream = chans;
            h.has_bno = bno;
            h.chip_id = chip;
            if nstr > 0 {
                h.first_stream_index = self.enabled_streams.len();
                self.enabled_streams.push(hs as u32);
                self.channels_per_stream.push(chans);
            }
            if nstr > 1 {
                self.enabled_streams.push(hs as u32 + 8);
                self.channels_per_stream.push(chans);
            }
        } else {
            h.num_streams = 0;
            h.has_bno = bno;
        }
    }

    /// Re-derive per-stream channel counts and first channels after the RHD2132 16-channel
    /// option changes, without rescanning. Parsing and layout only; no register writes.
    fn update_channel_counts(&mut self) {
        let mut first = 0;
        for (hs, h) in self.headstages.iter_mut().enumerate() {
            if h.num_streams == 0 {
                continue;
            }
            if h.chip_id == CHIP_ID_RHD2132 {
                h.channels_per_stream = if self.settings.rhd2132_16ch[hs] { 16 } else { 32 };
                for k in 0..h.num_streams {
                    self.channels_per_stream[h.first_stream_index + k] = h.channels_per_stream;
                }
            }
            h.first_channel = first;
            first += h.num_channels();
        }
    }

    fn update_board_streams(&mut self) -> Result<()> {
        for i in 0..MAX_STREAMS {
            if i < self.enabled_streams.len() {
                self.enable_data_stream(i, true)?;
                self.set_data_source(i, self.enabled_streams[i])?;
            } else {
                self.enable_data_stream(i, false)?;
            }
        }
        self.reset_board()?;
        Ok(())
    }

    fn scan_ports_impl(&mut self) -> Result<()> {
        self.check_board_mem()?;
        if self.has_i2c_support {
            let dev_i2c_enable = 0x1001;
            self.ctx.write_reg(DEVICE_INFO, dev_i2c_enable, 0xf)?;
            self.reset_board()?;
            let mut mask = 0;
            for p in 0..4u32 {
                let bno = self.bno_connected(p);
                self.has_bno[p as usize] = bno;
                let _ = self.ctx.write_reg(DEVICE_BNO_A + p * 2, 0, bno as u32);
                let i2c = self.ctx.read_reg(DEVICE_I2C_RAW_A + p * 2, 1 + (1 << 15)).map(|v| v > 0).unwrap_or(false);
                let id = if i2c { self.eeprom_device_id(p) } else { 0 };
                if bno {
                    let map = match id {
                        0 => {
                            debug!("BNO without EEPROM on port {p}; assuming low-profile 3D headstage");
                            0b00100100
                        }
                        0x8000_0002 | 0x8000_0003 => 0b10000011000,
                        _ => 0b00000100100,
                    };
                    let _ = self.ctx.write_reg(DEVICE_BNO_A + p * 2, 1, map);
                }
                if id != 0 {
                    info!("headstage EEPROM id {id:#x} on port {}", (b'A' + p as u8) as char);
                }
                if bno || (i2c && id != 0) {
                    mask |= 1 << p;
                }
            }
            self.ctx.write_reg(DEVICE_INFO, dev_i2c_enable, mask)?;
            self.reset_board()?;
        }

        self.enabled_streams.clear();
        self.channels_per_stream.clear();
        self.chip_ids.clear();
        for h in &mut self.headstages {
            h.num_streams = 0;
        }
        let current_rate = self.settings.sample_rate;
        self.set_sample_rate(30000, false)?;
        let init_ports = [0u32, 1, 2, 3, 4, 5, 6, 7]; // A1 A2 B1 B2 C1 C2 D1 D2
        for (i, &p) in init_ports.iter().enumerate() {
            self.set_data_source(i, p)?;
        }
        for i in 0..8 {
            self.enable_data_stream(i, true)?;
        }
        self.reset_board()?;
        self.select_aux_bank_all(AuxSlot::Cmd3, 0)?;
        self.set_max_timestep(128 * INIT_STEP as u32)?;
        self.set_continuous(false)?;

        let mut sum = [0; 8];
        let mut first = [-1; 8];
        let mut second = [-1; 8];
        let mut tmp_chip = [-1; 8];
        for delay in 0..16 {
            for p in 0..4 {
                self.set_cable_delay(p, delay)?;
            }
            self.run()?;
            let sb = self.read_scan_block(INIT_STEP);
            self.stop_hw()?;
            let sb = sb?;
            for hs in 0..8 {
                let (id, r59) = Self::intan_chip_id(&sb, hs);
                if Self::good_chip(id, r59) {
                    sum[hs] += 1;
                    if first[hs] == -1 {
                        first[hs] = delay;
                        tmp_chip[hs] = id;
                    } else if second[hs] == -1 {
                        second[hs] = delay;
                        tmp_chip[hs] = id;
                    }
                }
            }
        }

        for hs in 0..8 {
            let bno = self.has_bno[hs / 2];
            if tmp_chip[hs] > 0 && self.enabled_streams.len() < MAX_STREAMS {
                self.chip_ids.push(tmp_chip[hs]);
                if tmp_chip[hs] == CHIP_ID_RHD2164 {
                    if self.enabled_streams.len() < MAX_STREAMS - 1 {
                        self.enable_headstage(hs, true, 2, 32, bno, tmp_chip[hs]);
                        self.chip_ids.push(CHIP_ID_RHD2164_B);
                    } else {
                        self.enable_headstage(hs, true, 1, 32, bno, tmp_chip[hs]);
                    }
                } else {
                    let ch = if tmp_chip[hs] == CHIP_ID_RHD2132 { if self.settings.rhd2132_16ch[hs] { 16 } else { 32 } } else { 16 };
                    self.enable_headstage(hs, true, 1, ch, bno, tmp_chip[hs]);
                }
                info!("headstage {} found: {} ({} ch)", HS_NAMES[hs], chip_name(tmp_chip[hs]), self.headstages[hs].num_channels());
            } else if hs % 2 == 1 && bno {
                self.enable_headstage(hs, true, 0, 0, true, 0);
            } else {
                self.enable_headstage(hs, false, 0, 0, false, 0);
            }
        }
        self.update_board_streams()?;

        let mut opt = [0; 8];
        for hs in 0..8 {
            if sum[hs] == 1 || sum[hs] == 2 {
                opt[hs] = first[hs];
            } else if sum[hs] > 2 {
                opt[hs] = second[hs];
            }
        }
        for p in 0..4 {
            self.optimum_delay[p] = opt[2 * p].max(opt[2 * p + 1]);
        }
        for p in 0..4 {
            let d = self.delay_for_port(p, true);
            self.set_cable_delay(p, d)?;
            debug!("optimum delay port {}: {d}", (b'A' + p as u8) as char);
        }
        let rescan = !self.initial_scan;
        self.set_sample_rate(current_rate, rescan)?;
        self.initial_scan = false;
        Ok(())
    }

    fn electrode_to_stream(&self, electrode: usize) -> Option<(usize, usize)> {
        let mut acc = 0;
        for (i, &n) in self.channels_per_stream.iter().enumerate() {
            if electrode < acc + n {
                // Physical chip channel. (Stock omits the RHD2132 16-ch offset here, so its DAC
                // outputs an unwired channel in that mode.)
                return Some((i, electrode - acc + crate::parser::first_chip_channel(self.chip_ids[i], n)));
            }
            acc += n;
        }
        None
    }

    fn apply_live_settings(&mut self) -> Result<()> {
        let s = self.settings.clone();
        self.set_ttl_mode(s.ttl_output_mode as u32)?;
        self.enable_external_fast_settle(s.fast_ttl_settle.is_some())?;
        self.set_external_fast_settle_channel(s.fast_ttl_settle.unwrap_or(0) as u32)?;
        self.set_dac_hpf(s.dac_hpf_hz)?;
        self.enable_dac_hpf(s.dac_hpf_enabled)?;
        self.enable_leds(s.leds)?;
        self.set_clock_divider(s.clock_divider)?;
        self.set_audio_noise_suppress(s.noise_slicer)?;
        for d in 0..8 {
            self.apply_dac(d)?;
        }
        Ok(())
    }

    fn apply_dac(&self, d: usize) -> Result<()> {
        match self.dac_route[d] {
            Some((stream, ch, thr)) => {
                self.enable_dac(d, true)?;
                self.select_dac_stream(d, stream as u32)?;
                self.select_dac_channel(d, ch as u32)?;
                self.set_dac_threshold(d, ((thr as f64 / 0.195) + 32768.0).abs() as i32, thr >= 0.0)?;
            }
            None => self.enable_dac(d, false)?,
        }
        Ok(())
    }

    fn num_aux_channels(&self) -> usize {
        if !self.settings.acquire_aux {
            return 0;
        }
        3 * self.chip_ids.iter().filter(|&&c| c != CHIP_ID_RHD2164_B).count()
    }

    fn handle_side(&mut self, f: &Frame, blk: &mut Block) {
        let dev = f.dev_idx();
        let ts = f.time() as f64 / self.acq_clock_hz.max(1) as f64;
        if dev == DEVICE_MEMORY && self.has_memmon {
            let d = f.data();
            if d.len() >= 12 {
                let used = u32::from_le_bytes(d[8..12].try_into().unwrap());
                let pct = 100.0 * used as f32 / self.total_memory.max(1) as f32;
                self.health.as_ref().unwrap().hw_buffer_milli_pct.store((pct * 1000.0) as u64, Ordering::Relaxed);
                blk.side.push(SideSample { stream: 0, sample_number: self.mem_sample, timestamp: ts, values: vec![pct] });
                self.mem_sample += 1;
            }
            return;
        }
        for p in 0..4 {
            if self.has_bno[p] && dev == DEVICE_BNO_A + 2 * p as u32 {
                let d = f.data();
                if d.len() < 8 + 2 * 14 {
                    return;
                }
                let w = |i: usize| i16::from_le_bytes([d[8 + 2 * i], d[9 + 2 * i]]) as f32;
                let mut v = Vec::with_capacity(BNO_CHANNELS);
                (0..3).for_each(|i| v.push(w(i) / 16.0));
                (3..7).for_each(|i| v.push(w(i) / 16384.0));
                (7..13).for_each(|i| v.push(w(i) / 100.0));
                v.push(d[8 + 26] as f32);
                let cal = d[8 + 27];
                (0..4).for_each(|i| v.push(((cal >> (2 * i)) & 3) as f32));
                let stream = self.has_memmon as usize + (0..p).filter(|&q| self.has_bno[q]).count();
                blk.side.push(SideSample { stream, sample_number: self.bno_samples[p], timestamp: ts, values: v });
                self.bno_samples[p] += 1;
                return;
            }
        }
    }
}

pub fn chip_name(id: i32) -> &'static str {
    match id {
        CHIP_ID_RHD2132 => "RHD2132",
        CHIP_ID_RHD2216 => "RHD2216",
        CHIP_ID_RHD2164 => "RHD2164",
        _ => "unknown",
    }
}

impl Board for OniBoard {
    fn info(&self) -> BoardInfo {
        let (a, b, c, rc) = self.fw;
        BoardInfo {
            kind: "Open Ephys Acquisition Board (ONI/FT600)".into(),
            description: vec![
                ("Gateware".into(), format!("v{a}.{b}.{c}{}", if rc != 0 { format!("-rc{}", rc & 0x3f) } else { String::new() })),
                ("Board ID".into(), format!("{:#06x}", self.device_id)),
                ("Acq clock".into(), format!("{} Hz", self.acq_clock_hz)),
                ("Memory monitor".into(), if self.has_memmon { format!("{} words", self.total_memory) } else { "unsupported".into() }),
                ("Cable delays".into(), format!("{:?}", self.cable_delay)),
            ],
        }
    }

    fn layout(&self) -> StreamLayout {
        let mut channels = Vec::new();
        let mut headstages = Vec::new();
        for (hs, h) in self.headstages.iter().enumerate() {
            if h.num_streams == 0 {
                continue;
            }
            let idx = headstages.len();
            headstages.push(HeadstageInfo { name: HS_NAMES[hs].into(), slot: hs, port: hs / 2, chip: chip_name(h.chip_id).into(), num_channels: h.num_channels(), first_channel: h.first_channel, has_bno: h.has_bno });
            for _ in 0..h.num_channels() {
                let n = channels.len() + 1;
                channels.push(ChannelInfo { name: format!("CH{n}"), kind: ChannelKind::Electrode, bit_volts: ELECTRODE_UV_PER_BIT, units: "uV".into(), headstage: Some(idx) });
            }
        }
        for i in 0..self.num_aux_channels() {
            channels.push(ChannelInfo { name: format!("AUX{}", i + 1), kind: ChannelKind::Aux, bit_volts: AUX_V_PER_BIT, units: "V".into(), headstage: None });
        }
        if self.settings.acquire_adc {
            for i in 0..8 {
                channels.push(ChannelInfo { name: format!("ADC{}", i + 1), kind: ChannelKind::Adc, bit_volts: ADC_BIT_VOLTS as f32, units: "V".into(), headstage: None });
            }
        }
        let mut side_streams = Vec::new();
        if self.has_memmon {
            side_streams.push(SideStreamInfo { name: "memory_usage".into(), sample_rate: MEMORY_MONITOR_FS as f64, channel_names: vec!["MEM".into()], units: vec!["%".into()], bit_volts: vec![1.0] });
        }
        for p in 0..4 {
            if self.has_bno[p] {
                let names = ["Eul-Y", "Eul-R", "Eul-P", "Quat-W", "Quat-X", "Quat-Y", "Quat-Z", "Acc-X", "Acc-Y", "Acc-Z", "Grav-X", "Grav-Y", "Grav-Z", "Temp", "Cal-Mag", "Cal-Acc", "Cal-Gyr", "Cal-Sys"];
                let units = ["deg", "deg", "deg", "", "", "", "", "m/s^2", "m/s^2", "m/s^2", "m/s^2", "m/s^2", "m/s^2", "degC", "", "", "", ""];
                side_streams.push(SideStreamInfo { name: format!("IMU_port_{}", (b'A' + p as u8) as char), sample_rate: 100.0, channel_names: names.iter().map(|s| s.to_string()).collect(), units: units.iter().map(|s| s.to_string()).collect(), bit_volts: BNO_BIT_VOLTS.to_vec() });
            }
        }
        StreamLayout { name: "acquisition_board".into(), sample_rate: self.sample_rate, channels, headstages, ttl_lines: 8, side_streams, hw_clock_hz: self.acq_clock_hz as f64 }
    }

    fn settings(&self) -> &BoardSettings {
        &self.settings
    }

    fn apply_settings(&mut self, new: &BoardSettings) -> Result<()> {
        if self.running {
            bail!("cannot change board settings while acquiring");
        }
        let old = self.settings.clone();
        self.settings = new.clone();
        if new.rhd2132_16ch != old.rhd2132_16ch {
            self.update_channel_counts();
        }
        if new.usb_block_bytes != old.usb_block_bytes {
            self.reset_board()?;
        }
        if new.sample_rate != old.sample_rate {
            self.set_sample_rate(new.sample_rate, true)?;
        } else if new.cable_delay_adjust != old.cable_delay_adjust {
            self.check_all_cable_delays()?;
            self.update_registers()?;
        } else if (new.lower_bandwidth, new.upper_bandwidth, new.dsp_enabled, new.dsp_cutoff, new.acquire_aux, new.fast_settle) != (old.lower_bandwidth, old.upper_bandwidth, old.dsp_enabled, old.dsp_cutoff, old.acquire_aux, old.fast_settle) {
            self.update_registers()?;
        }
        self.apply_live_settings()?;
        Ok(())
    }

    fn available_sample_rates(&self) -> Vec<u32> {
        SAMPLE_RATES.iter().map(|r| r.0).collect()
    }

    fn scan_ports(&mut self) -> Result<()> {
        if self.running {
            bail!("cannot scan while acquiring");
        }
        self.scan_ports_impl()
    }

    fn start(&mut self, health: SharedHealth) -> Result<()> {
        if self.enabled_streams.is_empty() && !self.has_bno.iter().any(|&b| b) {
            bail!("no headstages detected");
        }
        self.check_board_mem()?;
        self.apply_live_settings()?;
        self.ttl_out = 0;
        self.write_ttl_out(0)?;
        let nstreams = self.enabled_streams.len();
        self.parser = RhythmParser::new(self.channels_per_stream.clone(), self.chip_ids.clone(), self.settings.acquire_aux, self.settings.acquire_adc, self.device_id == DEVICE_ID_V3);
        self.pending = None;
        self.mem_sample = 0;
        self.bno_samples = [0; 4];
        health.reset();
        self.health = Some(health);
        self.set_continuous(true)?;
        self.run()?;
        self.running = true;
        info!("acquisition started: {} streams, {} channels", nstreams, self.layout().num_channels());
        Ok(())
    }

    fn read_block(&mut self, blk: &mut Block) -> Result<()> {
        blk.clear();
        blk.num_channels = self.parser.num_channels();
        let target = self.settings.block_samples.max(1);
        let has_rhythm = !self.enabled_streams.is_empty();
        let health = self.health.clone().unwrap();
        loop {
            let ctx = &self.ctx;
            let next = || {
                ctx.read_frame().inspect_err(|_| {
                    health.read_errors.fetch_add(1, Ordering::Relaxed);
                })
            };
            match crate::assemble::assemble(&mut self.parser, &mut self.pending, blk, target, has_rhythm, self.acq_clock_hz, &health, next)? {
                Assembled::Block => return Ok(()),
                Assembled::Side(f) => {
                    self.handle_side(&f, blk);
                    // BNO-only configurations still need to deliver blocks.
                    if !has_rhythm && blk.side.len() >= 4 {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn live(&mut self, cmd: &LiveCommand) -> Result<()> {
        match *cmd {
            LiveCommand::SetTtlOut(w) => {
                self.ttl_out = w;
                self.write_ttl_out(w)?;
            }
            LiveCommand::DacRoute { dac, electrode, threshold_uv } => {
                if dac < 8 {
                    self.dac_route[dac] = electrode.and_then(|e| self.electrode_to_stream(e)).map(|(s, c)| (s, c, threshold_uv));
                    self.apply_dac(dac)?;
                }
            }
            LiveCommand::SetLeds(on) => {
                self.settings.leds = on;
                self.enable_leds(on)?;
            }
            LiveCommand::SetDacHpf { enabled, hz } => {
                self.settings.dac_hpf_enabled = enabled;
                self.settings.dac_hpf_hz = hz;
                self.set_dac_hpf(hz)?;
                self.enable_dac_hpf(enabled)?;
            }
            LiveCommand::SetNoiseSlicer(n) => {
                self.settings.noise_slicer = n;
                self.set_audio_noise_suppress(n)?;
            }
            LiveCommand::SetTtlMode(on) => {
                self.settings.ttl_output_mode = on;
                self.set_ttl_mode(on as u32)?;
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if !self.running {
            return Ok(());
        }
        self.pending = None;
        self.running = false;
        self.set_continuous(false)?;
        self.set_max_timestep(0)?;
        self.stop_hw()?;
        self.reset_board()?;
        info!("acquisition stopped");
        Ok(())
    }
}

