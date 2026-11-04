//! Dumps command lists in the same format as the C++ reference harness (for diffing).
use rhythm::registers::Rhd2000Registers;
fn main() {
    let rates = [1000.0, 1250.0, 1500.0, 2000.0, 2500.0, 3000.0, 10000.0 / 3.0, 5000.0, 6250.0, 10000.0, 12500.0, 15000.0, 20000.0, 25000.0, 30000.0];
    let p = |c: &[i32]| { for x in c { print!("{x} "); } println!(); };
    for r in rates { for u in [100.0, 3000.0, 7500.0, 20000.0] { for l in [0.1, 1.0, 10.0, 300.0] { for d in [0.1, 0.5, 5.0, 200.0] { for aux in [false, true] { for dsp in [false, true] {
        let mut g = Rhd2000Registers::new(30000.0);
        g.define_sample_rate(r);
        let dd = g.set_dsp_cutoff_freq(d); let ll = g.set_lower_bandwidth(l); let uu = g.set_upper_bandwidth(u);
        g.enable_dsp(dsp); g.enable_aux(aux);
        println!("{r:.4} {dd:.6} {ll:.6} {uu:.6}");
        p(&g.command_list_register_config(false)); p(&g.command_list_register_config(true));
        g.set_fast_settle(true); p(&g.command_list_register_config(false)); g.set_fast_settle(false);
        p(&g.command_list_temp_sensor());
        g.set_dig_out_low(); p(&g.command_list_update_dig_out());
        match g.command_list_zcheck_dac(1000.0, 128.0) { Some(c) => p(&c), None => println!() }
    }}}}}}
}
