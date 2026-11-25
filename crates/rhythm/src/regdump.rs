//! Text dump of the register and command lists the headstage chips are programmed with, in the
//! same format as the C++ reference harness (for diffing). `examples/regdump.rs` prints it; the
//! test below compares its digest with the checked-in `testdata/regdump-digest.txt`.

use crate::registers::Rhd2000Registers;
use std::fmt::Write;

/// Appends one command list as space-separated values and a newline.
fn push_list(out: &mut String, list: &[i32]) {
    for x in list {
        write!(out, "{x} ").unwrap();
    }
    out.push('\n');
}

/// The whole dump: for each (sample rate, upper bandwidth, lower bandwidth, DSP cutoff, aux,
/// dsp) setting, one header line followed by the command lists that setting produces.
pub fn dump() -> String {
    let rates = [
        1000.0,
        1250.0,
        1500.0,
        2000.0,
        2500.0,
        3000.0,
        10000.0 / 3.0,
        5000.0,
        6250.0,
        10000.0,
        12500.0,
        15000.0,
        20000.0,
        25000.0,
        30000.0,
    ];
    let mut out = String::new();
    for r in rates {
        for u in [100.0, 3000.0, 7500.0, 20000.0] {
            for l in [0.1, 1.0, 10.0, 300.0] {
                for d in [0.1, 0.5, 5.0, 200.0] {
                    for aux in [false, true] {
                        for dsp in [false, true] {
                            let mut g = Rhd2000Registers::new(30000.0);
                            g.define_sample_rate(r);
                            let dd = g.set_dsp_cutoff_freq(d);
                            let ll = g.set_lower_bandwidth(l);
                            let uu = g.set_upper_bandwidth(u);
                            g.enable_dsp(dsp);
                            g.enable_aux(aux);
                            writeln!(out, "{r:.4} {dd:.6} {ll:.6} {uu:.6}").unwrap();
                            push_list(&mut out, &g.command_list_register_config(false));
                            push_list(&mut out, &g.command_list_register_config(true));
                            g.set_fast_settle(true);
                            push_list(&mut out, &g.command_list_register_config(false));
                            g.set_fast_settle(false);
                            push_list(&mut out, &g.command_list_temp_sensor());
                            g.set_dig_out_low();
                            push_list(&mut out, &g.command_list_update_dig_out());
                            match g.command_list_zcheck_dac(1000.0, 128.0) {
                                Some(c) => push_list(&mut out, &c),
                                None => out.push('\n'),
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Lines per setting in [`dump`]: the header and six command lists.
const LINES_PER_SETTING: usize = 7;

/// [`dump`] in short: per setting, its header line and a hash (FNV-1a, 64 bits) of its six
/// command lists. This is what `testdata/regdump-digest.txt` holds (the dump itself is 7 MB).
pub fn digest() -> String {
    let text = dump();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len().is_multiple_of(LINES_PER_SETTING));
    let mut out = String::new();
    for setting in lines.chunks(LINES_PER_SETTING) {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for line in &setting[1..] {
            for b in line.bytes().chain([b'\n']) {
                h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        writeln!(out, "{} {h:016x}", setting[0]).unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::digest;

    const GOLDEN: &str = include_str!("../testdata/regdump-digest.txt");

    /// The register lists are what gets programmed into the headstage chips. Any change to them
    /// must be checked against the stock Open Ephys plugin before the golden file is updated.
    #[test]
    fn register_lists_match_the_golden_digest() {
        let got = digest();
        assert_eq!(
            got.lines().count(),
            GOLDEN.lines().count(),
            "the number of settings dumped changed"
        );
        for (n, (g, w)) in got.lines().zip(GOLDEN.lines()).enumerate() {
            assert!(
                g == w,
                "the register lists changed, first at setting {} (sample rate, DSP cutoff, lower and upper \
                 bandwidth, hash):\n  now:    {g}\n  golden: {w}\n\
                 `cargo run --release -p rhythm --example regdump` prints the lists in full. Regenerate the \
                 golden file only after checking the change against the stock Open Ephys plugin:\n  \
                 cargo run --release -p rhythm --example regdump -- --digest > crates/rhythm/testdata/regdump-digest.txt",
                n + 1
            );
        }
        assert!(
            got == GOLDEN,
            "the digest differs from the golden file only in line endings"
        );
    }
}
