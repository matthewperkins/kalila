//! Port of Intan's `Rhd2000Registers` (as used by the Open Ephys ONI acquisition
//! board plugin). Generates the auxiliary SPI command lists uploaded to the FPGA.
//!
//! This is a deliberately literal port: register values and command order are
//! identical to the C++ so that the chips are configured exactly as they are by
//! the stock Open Ephys plugin.

use std::f64::consts::PI;

pub const MAX_COMMAND_LENGTH: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZcheckCs {
    Cs100fF,
    Cs1pF,
    Cs10pF,
}

#[derive(Clone, Debug)]
pub struct Rhd2000Registers {
    sample_rate: f64,
    adc_reference_bw: i32,
    amp_fast_settle: i32,
    amp_vref_enable: i32,
    adc_comparator_bias: i32,
    adc_comparator_select: i32,
    vdd_sense_enable: i32,
    adc_buffer_bias: i32,
    mux_bias: i32,
    mux_load: i32,
    temp_s1: i32,
    temp_s2: i32,
    temp_en: i32,
    dig_out_hiz: i32,
    dig_out: i32,
    weak_miso: i32,
    twos_comp: i32,
    abs_mode: i32,
    dsp_en: i32,
    dsp_cutoff_freq: i32,
    zcheck_dac_power: i32,
    zcheck_load: i32,
    zcheck_scale: i32,
    zcheck_conn_all: i32,
    zcheck_sel_pol: i32,
    zcheck_en: i32,
    zcheck_select: i32,
    off_chip_rh1: i32,
    off_chip_rh2: i32,
    off_chip_rl: i32,
    adc_aux1_en: i32,
    adc_aux2_en: i32,
    adc_aux3_en: i32,
    rh1_dac1: i32,
    rh1_dac2: i32,
    rh2_dac1: i32,
    rh2_dac2: i32,
    rl_dac1: i32,
    rl_dac2: i32,
    rl_dac3: i32,
    a_pwr: [i32; 64],
}

pub fn cmd_convert(channel: i32) -> i32 {
    assert!((0..=63).contains(&channel));
    channel << 8
}
pub fn cmd_reg_read(reg: i32) -> i32 {
    assert!((0..=63).contains(&reg));
    0xc000 + (reg << 8)
}
pub fn cmd_reg_write(reg: i32, data: i32) -> i32 {
    assert!((0..=63).contains(&reg) && (0..=255).contains(&data));
    0x8000 + (reg << 8) + data
}
pub const CMD_CALIBRATE: i32 = 0x5500;
pub const CMD_CAL_CLEAR: i32 = 0x6a00;

impl Rhd2000Registers {
    pub fn new(sample_rate: f64) -> Self {
        let mut r = Self {
            sample_rate,
            adc_reference_bw: 3,
            amp_fast_settle: 0,
            amp_vref_enable: 1,
            adc_comparator_bias: 3,
            adc_comparator_select: 2,
            vdd_sense_enable: 1,
            adc_buffer_bias: 0,
            mux_bias: 0,
            mux_load: 0,
            temp_s1: 0,
            temp_s2: 0,
            temp_en: 0,
            dig_out_hiz: 1,
            dig_out: 0,
            weak_miso: 1,
            twos_comp: 0,
            abs_mode: 0,
            dsp_en: 1,
            dsp_cutoff_freq: 0,
            zcheck_dac_power: 1,
            zcheck_load: 0,
            zcheck_scale: 0,
            zcheck_conn_all: 0,
            zcheck_sel_pol: 0,
            zcheck_en: 0,
            zcheck_select: 0,
            off_chip_rh1: 0,
            off_chip_rh2: 0,
            off_chip_rl: 0,
            adc_aux1_en: 1,
            adc_aux2_en: 1,
            adc_aux3_en: 1,
            rh1_dac1: 0,
            rh1_dac2: 0,
            rh2_dac1: 0,
            rh2_dac2: 0,
            rl_dac1: 0,
            rl_dac2: 0,
            rl_dac3: 0,
            a_pwr: [1; 64],
        };
        r.define_sample_rate(sample_rate);
        r.set_dsp_cutoff_freq(1.0);
        r.set_upper_bandwidth(10000.0);
        r.set_lower_bandwidth(1.0);
        r
    }

    pub fn define_sample_rate(&mut self, sr: f64) {
        self.sample_rate = sr;
        self.mux_load = 0;
        let (mb, ab) = if sr < 3334.0 {
            (40, 32)
        } else if sr < 4001.0 {
            (40, 16)
        } else if sr < 5001.0 {
            (40, 8)
        } else if sr < 6251.0 {
            (32, 8)
        } else if sr < 8001.0 {
            (26, 8)
        } else if sr < 10001.0 {
            (18, 4)
        } else if sr < 12501.0 {
            (16, 3)
        } else if sr < 15001.0 {
            (7, 3)
        } else {
            (4, 2)
        };
        self.mux_bias = mb;
        self.adc_buffer_bias = ab;
    }

    pub fn set_fast_settle(&mut self, on: bool) {
        self.amp_fast_settle = on as i32;
    }
    pub fn set_dig_out_low(&mut self) {
        self.dig_out = 0;
        self.dig_out_hiz = 0;
    }
    pub fn set_dig_out_high(&mut self) {
        self.dig_out = 1;
        self.dig_out_hiz = 0;
    }
    pub fn set_dig_out_hiz(&mut self) {
        self.dig_out = 0;
        self.dig_out_hiz = 1;
    }
    pub fn enable_aux(&mut self, on: bool) {
        self.adc_aux1_en = on as i32;
        self.adc_aux2_en = on as i32;
        self.adc_aux3_en = on as i32;
    }
    pub fn enable_dsp(&mut self, on: bool) {
        self.dsp_en = on as i32;
    }
    pub fn enable_zcheck(&mut self, on: bool) {
        self.zcheck_en = on as i32;
    }
    pub fn set_zcheck_dac_power(&mut self, on: bool) {
        self.zcheck_dac_power = on as i32;
    }
    pub fn set_zcheck_scale(&mut self, s: ZcheckCs) {
        self.zcheck_scale = match s {
            ZcheckCs::Cs100fF => 0,
            ZcheckCs::Cs1pF => 1,
            ZcheckCs::Cs10pF => 3,
        };
    }
    pub fn set_zcheck_polarity_negative(&mut self, neg: bool) {
        self.zcheck_sel_pol = neg as i32;
    }
    pub fn set_zcheck_channel(&mut self, ch: i32) -> i32 {
        if !(0..=63).contains(&ch) {
            return -1;
        }
        self.zcheck_select = ch;
        ch
    }
    pub fn set_amp_powered(&mut self, ch: usize, on: bool) {
        if ch < 64 {
            self.a_pwr[ch] = on as i32;
        }
    }

    fn dsp_fcutoff(&self, n: i32) -> f64 {
        let x = 2f64.powi(n);
        self.sample_rate * (x / (x - 1.0)).ln() / (2.0 * PI)
    }

    /// Returns the actual cutoff frequency chosen.
    pub fn set_dsp_cutoff_freq(&mut self, f: f64) -> f64 {
        let f1 = self.dsp_fcutoff(1);
        let f15 = self.dsp_fcutoff(15);
        if f > f1 {
            self.dsp_cutoff_freq = 1;
        } else if f < f15 {
            self.dsp_cutoff_freq = 15;
        } else {
            let lf = f.log10();
            let mut best = 1e7;
            for n in 1..16 {
                let d = (lf - self.dsp_fcutoff(n).log10()).abs();
                if d < best {
                    best = d;
                    self.dsp_cutoff_freq = n;
                }
            }
        }
        self.dsp_fcutoff(self.dsp_cutoff_freq)
    }
    pub fn dsp_cutoff_freq(&self) -> f64 {
        self.dsp_fcutoff(self.dsp_cutoff_freq)
    }

    pub fn register_value(&self, reg: i32) -> i32 {
        let p = &self.a_pwr;
        let byte = |b: usize| -> i32 { (0..8).map(|i| p[b + i] << i).sum() };
        match reg {
            0 => {
                (self.adc_reference_bw << 6)
                    + (self.amp_fast_settle << 5)
                    + (self.amp_vref_enable << 4)
                    + (self.adc_comparator_bias << 2)
                    + self.adc_comparator_select
            }
            1 => (self.vdd_sense_enable << 6) + self.adc_buffer_bias,
            2 => self.mux_bias,
            3 => {
                (self.mux_load << 5)
                    + (self.temp_s2 << 4)
                    + (self.temp_s1 << 3)
                    + (self.temp_en << 2)
                    + (self.dig_out_hiz << 1)
                    + self.dig_out
            }
            4 => {
                (self.weak_miso << 7)
                    + (self.twos_comp << 6)
                    + (self.abs_mode << 5)
                    + (self.dsp_en << 4)
                    + self.dsp_cutoff_freq
            }
            5 => {
                (self.zcheck_dac_power << 6)
                    + (self.zcheck_load << 5)
                    + (self.zcheck_scale << 3)
                    + (self.zcheck_conn_all << 2)
                    + (self.zcheck_sel_pol << 1)
                    + self.zcheck_en
            }
            6 => 128,
            7 => self.zcheck_select,
            8 => (self.off_chip_rh1 << 7) + self.rh1_dac1,
            9 => (self.adc_aux1_en << 7) + self.rh1_dac2,
            10 => (self.off_chip_rh2 << 7) + self.rh2_dac1,
            11 => (self.adc_aux2_en << 7) + self.rh2_dac2,
            12 => (self.off_chip_rl << 7) + self.rl_dac1,
            13 => (self.adc_aux3_en << 7) + (self.rl_dac3 << 6) + self.rl_dac2,
            14 => byte(0),
            // NB: upstream Intan code uses aPwr[0] for bit 0 of register 15 instead of
            // aPwr[8]. Reproduced verbatim (all amps are powered in practice).
            15 => (byte(8) & !1) | p[0],
            16 => byte(16),
            17 => byte(24),
            18 => byte(32),
            19 => byte(40),
            20 => byte(48),
            21 => byte(56),
            _ => -1,
        }
    }

    fn rh1_from_upper(f: f64) -> f64 {
        let l = f.log10();
        0.9730 * 10f64.powf(8.0968 - 1.1892 * l + 0.04767 * l * l)
    }
    fn rh2_from_upper(f: f64) -> f64 {
        let l = f.log10();
        1.0191 * 10f64.powf(8.1009 - 1.0821 * l + 0.03383 * l * l)
    }
    fn rl_from_lower(f: f64) -> f64 {
        let l = f.log10();
        if f < 4.0 {
            1.0061 * 10f64.powf(4.9391 - 1.2088 * l + 0.5698 * l * l + 0.1442 * l * l * l)
        } else {
            1.0061 * 10f64.powf(4.7351 - 0.5916 * l + 0.08482 * l * l)
        }
    }
    fn quad(a: f64, b: f64, c: f64) -> f64 {
        10f64.powf((-b - (b * b - 4.0 * a * c).sqrt()) / (2.0 * a))
    }
    fn upper_from_rh1(r: f64) -> f64 {
        Self::quad(0.04767, -1.1892, 8.0968 - (r / 0.9730).log10())
    }
    fn upper_from_rh2(r: f64) -> f64 {
        Self::quad(0.03383, -1.0821, 8.1009 - (r / 1.0191).log10())
    }
    fn lower_from_rl(mut r: f64) -> f64 {
        if r < 5100.0 {
            r = 5100.0;
        }
        if r < 30000.0 {
            Self::quad(0.08482, -0.5916, 4.7351 - (r / 1.0061).log10())
        } else {
            Self::quad(0.3303, -1.2100, 4.9873 - (r / 1.0061).log10())
        }
    }

    pub fn set_upper_bandwidth(&mut self, mut f: f64) -> f64 {
        if f > 30000.0 {
            f = 30000.0;
        }
        let fit = |target: f64, base: f64, u1: f64, u2: f64| -> (i32, i32, f64) {
            let (mut d1, mut d2, mut act) = (0, 0, base);
            for _ in 0..31 {
                if act < target - (u2 - u1 / 2.0) {
                    act += u2;
                    d2 += 1;
                }
            }
            for _ in 0..63 {
                if act < target - (u1 / 2.0) {
                    act += u1;
                    d1 += 1;
                }
            }
            (d1, d2, act)
        };
        let (a1, a2, r1) = fit(Self::rh1_from_upper(f), 2200.0, 600.0, 29400.0);
        let (b1, b2, r2) = fit(Self::rh2_from_upper(f), 8700.0, 763.0, 38400.0);
        self.rh1_dac1 = a1;
        self.rh1_dac2 = a2;
        self.rh2_dac1 = b1;
        self.rh2_dac2 = b2;
        (Self::upper_from_rh1(r1) * Self::upper_from_rh2(r2)).sqrt()
    }

    pub fn set_lower_bandwidth(&mut self, mut f: f64) -> f64 {
        const BASE: f64 = 3500.0;
        const U1: f64 = 175.0;
        const U2: f64 = 12700.0;
        const U3: f64 = 3_000_000.0;
        if f > 1500.0 {
            f = 1500.0;
        }
        let target = Self::rl_from_lower(f);
        self.rl_dac1 = 0;
        self.rl_dac2 = 0;
        self.rl_dac3 = 0;
        let mut act = BASE;
        if f < 0.15 {
            act += U3;
            self.rl_dac3 += 1;
        }
        for _ in 0..63 {
            if act < target - (U2 - U1 / 2.0) {
                act += U2;
                self.rl_dac2 += 1;
            }
        }
        for _ in 0..127 {
            if act < target - (U1 / 2.0) {
                act += U1;
                self.rl_dac1 += 1;
            }
        }
        Self::lower_from_rl(act)
    }

    pub fn set_lower_bandwidth_dac_values(&mut self, d1: i32, d2: i32, d3: i32) -> f64 {
        self.rl_dac1 = d1.clamp(0, 127);
        self.rl_dac2 = d2.clamp(0, 63);
        self.rl_dac3 = (d3 > 0) as i32;
        self.lower_bandwidth()
    }
    pub fn lower_bandwidth_dac_values(&self) -> (i32, i32, i32) {
        (self.rl_dac1, self.rl_dac2, self.rl_dac3)
    }
    pub fn lower_bandwidth(&self) -> f64 {
        Self::lower_from_rl(
            3500.0 + self.rl_dac1 as f64 * 175.0 + self.rl_dac2 as f64 * 12700.0 + self.rl_dac3 as f64 * 3_000_000.0,
        )
    }

    pub fn command_list_register_config(&self, calibrate: bool) -> Vec<i32> {
        let mut c = Vec::with_capacity(64);
        c.push(cmd_reg_read(63));
        c.push(cmd_reg_read(63));
        for r in [0, 1, 2, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17] {
            c.push(cmd_reg_write(r, self.register_value(r)));
        }
        for r in [63, 62, 61, 60, 59, 48, 49, 50, 51, 52, 53, 54, 55, 40, 41, 42, 43, 44] {
            c.push(cmd_reg_read(r));
        }
        for r in 0..=17 {
            c.push(cmd_reg_read(r));
        }
        c.push(if calibrate { CMD_CALIBRATE } else { cmd_reg_read(63) });
        for r in 18..=21 {
            c.push(cmd_reg_write(r, self.register_value(r)));
        }
        c.push(cmd_reg_read(63));
        c
    }

    pub fn command_list_temp_sensor(&mut self) -> Vec<i32> {
        let mut c = Vec::with_capacity(64);
        let aux = |c: &mut Vec<i32>| {
            c.push(cmd_convert(32));
            c.push(cmd_convert(33));
            c.push(cmd_convert(34));
        };
        self.temp_en = 1;
        aux(&mut c);
        self.temp_s1 = self.temp_en;
        self.temp_s2 = 0;
        c.push(cmd_reg_write(3, self.register_value(3)));
        aux(&mut c);
        self.temp_s1 = self.temp_en;
        self.temp_s2 = self.temp_en;
        c.push(cmd_reg_write(3, self.register_value(3)));
        aux(&mut c);
        c.push(cmd_convert(49));
        aux(&mut c);
        self.temp_s1 = 0;
        self.temp_s2 = self.temp_en;
        c.push(cmd_reg_write(3, self.register_value(3)));
        aux(&mut c);
        c.push(cmd_convert(49));
        aux(&mut c);
        self.temp_s1 = 0;
        self.temp_s2 = 0;
        c.push(cmd_reg_write(3, self.register_value(3)));
        aux(&mut c);
        c.push(cmd_convert(48));
        for _ in 0..8 {
            aux(&mut c);
            c.push(cmd_reg_read(63));
        }
        c
    }

    pub fn command_list_update_dig_out(&mut self) -> Vec<i32> {
        let mut c = Vec::with_capacity(64);
        let w = |s: &Self, n: usize, c: &mut Vec<i32>| {
            for _ in 0..n {
                c.push(cmd_reg_write(3, s.register_value(3)));
            }
        };
        self.temp_en = 1;
        w(self, 3, &mut c);
        self.temp_s1 = self.temp_en;
        self.temp_s2 = 0;
        w(self, 4, &mut c);
        self.temp_s1 = self.temp_en;
        self.temp_s2 = self.temp_en;
        w(self, 8, &mut c);
        self.temp_s1 = 0;
        self.temp_s2 = self.temp_en;
        w(self, 8, &mut c);
        self.temp_s1 = 0;
        self.temp_s2 = 0;
        w(self, 5, &mut c);
        w(self, 32, &mut c);
        c
    }

    /// Impedance-test waveform on the on-chip Zcheck DAC.
    pub fn command_list_zcheck_dac(&self, frequency: f64, amplitude: f64) -> Option<Vec<i32>> {
        if !(0.0..=128.0).contains(&amplitude) || frequency < 0.0 || frequency > self.sample_rate / 4.0 {
            return None;
        }
        if frequency == 0.0 {
            return Some(vec![cmd_reg_write(6, 128); MAX_COMMAND_LENGTH]);
        }
        let period = (self.sample_rate / frequency + 0.5).floor() as usize;
        if period > MAX_COMMAND_LENGTH {
            return None;
        }
        let mut t = 0.0;
        let mut c = Vec::with_capacity(period);
        for _ in 0..period {
            let v = ((amplitude * (2.0 * PI * frequency * t).sin() + 128.0 + 0.5).floor() as i32).clamp(0, 255);
            c.push(cmd_reg_write(6, v));
            t += 1.0 / self.sample_rate;
        }
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_list_lengths_match_upstream() {
        let mut r = Rhd2000Registers::new(30000.0);
        assert_eq!(r.command_list_register_config(true).len(), 60);
        assert_eq!(r.command_list_temp_sensor().len(), 60);
        assert_eq!(r.command_list_update_dig_out().len(), 60);
    }

    #[test]
    fn default_bandwidths_are_sane() {
        let mut r = Rhd2000Registers::new(30000.0);
        let up = r.set_upper_bandwidth(7500.0);
        let lo = r.set_lower_bandwidth(1.0);
        assert!((up - 7500.0).abs() / 7500.0 < 0.1, "{up}");
        assert!((lo - 1.0).abs() < 0.2, "{lo}");
        let dsp = r.set_dsp_cutoff_freq(0.5);
        assert!(dsp > 0.1 && dsp < 2.0, "{dsp}");
    }

    #[test]
    fn register0_matches_known_value() {
        let r = Rhd2000Registers::new(30000.0);
        // adcRefBw=3, fastSettle=0, vref=1, cmpBias=3, cmpSel=2 -> 0b11011110
        assert_eq!(r.register_value(0), 0xDE);
    }
}
