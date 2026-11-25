//! The DAC / TTL outputs: what the board was last told, the panel, and the live commands that
//! bring the board up to date (`plan_outputs`, a pure function so it can be tested).

use crate::config::OutputSettings;
use eframe::egui::{self, RichText};
use engine::rhythm::LiveCommand;

/// What the panel needs from the rest of the app, read only when the panel is open.
pub struct OutputsEnv {
    /// Electrode names, by global electrode index.
    pub names: Vec<String>,
    /// First channel on the audio monitor (DAC 1 follows it).
    pub audio_first: Option<usize>,
}

/// What the panel did this frame.
pub struct OutputsUi {
    /// A setting was edited (the config needs saving).
    pub changed: bool,
    /// Manual TTL-out words to send, in order.
    pub ttl_out: Vec<u16>,
}

/// Output-sync state: the manual TTL-out word and what the board last got.
#[derive(Default)]
pub struct Outputs {
    /// Manual TTL-out word (reset to 0 at every acquisition start, as the board does).
    ttl_out_word: u16,
    sent: SentOutputs,
}

impl Outputs {
    /// An acquisition starts: the board resets TTL out to 0.
    pub fn acquisition_started(&mut self) {
        self.ttl_out_word = 0;
    }

    /// The live commands that bring the board up to date, and whether the board-level ones are
    /// among them (see `plan_outputs`).
    pub fn plan(
        &mut self,
        o: &OutputSettings,
        nel: usize,
        audio_first: Option<usize>,
        force: bool,
    ) -> (Vec<LiveCommand>, bool) {
        plan_outputs(&mut self.sent, o, nel, audio_first, force)
    }

    /// DAC/TTL outputs. All live: sent while acquiring, and again at every acquisition start
    /// (`sync_outputs`). Writes only the board's DAC-select/threshold, HPF, DAC-control
    /// (noise slicer), MODE (TTL-out mode) registers and the TTL-out device.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        o: &mut OutputSettings,
        acquiring: bool,
        env: impl FnOnce() -> OutputsEnv,
    ) -> OutputsUi {
        let mut res = OutputsUi {
            changed: false,
            ttl_out: Vec::new(),
        };
        egui::CollapsingHeader::new(RichText::new("Outputs (DAC / TTL)").strong()).default_open(false).show(ui, |ui| {
            let OutputsEnv { names, audio_first } = env();
            let old = o.clone();
            ui.small("Applied live while acquiring and re-sent at every start.");
            ui.checkbox(&mut o.dac1_follows_audio, "DAC 1 follows audio monitor").on_hover_text("DAC 1 (board audio L) plays the first channel on the audio monitor");
            egui::Grid::new("dacs").num_columns(3).spacing([6.0, 3.0]).show(ui, |ui| {
                for d in 0..8 {
                    ui.label(format!("DAC {}{}", d + 1, match d {
                        0 => " L",
                        1 => " R",
                        _ => "",
                    }))
                    .on_hover_text(if d < 2 { "Also the board's audio output (noise slicer applies)" } else { "Analog output" });
                    let follow = d == 0 && o.dac1_follows_audio;
                    let cur = if follow { audio_first } else { o.dacs[d].electrode };
                    let text = match cur {
                        Some(e) => names.get(e).cloned().unwrap_or_else(|| format!("#{}", e + 1)),
                        None => "off".into(),
                    };
                    // Monospace while following: the proportional font has no ♪ (it drew an empty box).
                    ui.add_enabled_ui(!follow, |ui| {
                        egui::ComboBox::from_id_salt(("dac", d)).width(64.0).selected_text(if follow { RichText::new(format!("♪ {text}")).monospace() } else { RichText::new(text) }).show_ui(ui, |ui| {
                            ui.selectable_value(&mut o.dacs[d].electrode, None, "off");
                            for (i, n) in names.iter().enumerate() {
                                ui.selectable_value(&mut o.dacs[d].electrode, Some(i), n);
                            }
                        });
                    });
                    ui.add(egui::DragValue::new(&mut o.dacs[d].threshold_uv).range(-5000.0..=5000.0).speed(1.0).suffix(" µV"))
                        .on_hover_text("Comparator threshold: ≥ 0 fires at or above it, < 0 at or below it. Drives TTL out n in threshold mode.");
                    ui.end_row();
                }
            });
            ui.checkbox(&mut o.ttl_threshold_mode, "Threshold → TTL out").on_hover_text("TTL out n goes high while DAC n crosses its threshold (stock \"DAC TTL\")");
            ui.horizontal(|ui| {
                ui.checkbox(&mut o.dac_hpf_enabled, "DAC high-pass");
                ui.add_enabled(o.dac_hpf_enabled, egui::DragValue::new(&mut o.dac_hpf_hz).range(1.0..=5000.0).speed(5.0).suffix(" Hz"));
            });
            ui.add(egui::Slider::new(&mut o.noise_slicer, 0..=127).text("noise slicer (DAC L/R)"));
            ui.horizontal(|ui| {
                ui.label("TTL out");
                let manual = acquiring && !o.ttl_threshold_mode;
                for i in 0..8 {
                    let on = self.ttl_out_word & (1 << i) != 0;
                    let b = egui::Button::new(RichText::new(format!("{}", i + 1)).monospace()).selected(on).min_size(egui::vec2(18.0, 0.0));
                    if ui.add_enabled(manual, b).on_hover_text("Toggle TTL output line").clicked() {
                        self.ttl_out_word ^= 1 << i;
                        res.ttl_out.push(self.ttl_out_word);
                    }
                }
            });
            if o.ttl_threshold_mode {
                ui.weak("TTL out follows the DAC comparators (manual off)");
            } else if !acquiring {
                ui.weak("Manual TTL out needs acquisition running");
            }
            if *o != old {
                res.changed = true;
            }
        });
        res
    }
}

/// What the board was last told about its outputs.
#[derive(Default)]
struct SentOutputs {
    /// Last DAC route sent per DAC: (electrode, threshold µV).
    dac: [Option<(Option<usize>, f32)>; 8],
    /// Last board-level output settings sent.
    board: Option<(bool, bool, u64, i32)>,
}

/// The live commands `sync_outputs` sends, in order, and whether the board-level ones are among
/// them (the caller then mirrors them into the board settings). Updates `sent` as if they were sent.
fn plan_outputs(
    sent: &mut SentOutputs,
    o: &OutputSettings,
    nel: usize,
    audio_first: Option<usize>,
    force: bool,
) -> (Vec<LiveCommand>, bool) {
    let mut cmds = Vec::new();
    for d in 0..8 {
        let e = if d == 0 && o.dac1_follows_audio {
            audio_first
        } else {
            o.dacs[d].electrode
        };
        let want = (e.filter(|&e| e < nel), o.dacs[d].threshold_uv);
        if force || sent.dac[d] != Some(want) {
            cmds.push(LiveCommand::DacRoute {
                dac: d,
                electrode: want.0,
                threshold_uv: want.1,
            });
            sent.dac[d] = Some(want);
        }
    }
    let want = (
        o.ttl_threshold_mode,
        o.dac_hpf_enabled,
        o.dac_hpf_hz.to_bits(),
        o.noise_slicer,
    );
    let board_level = force || sent.board != Some(want);
    if board_level {
        cmds.push(LiveCommand::SetTtlMode(o.ttl_threshold_mode));
        cmds.push(LiveCommand::SetDacHpf {
            enabled: o.dac_hpf_enabled,
            hz: o.dac_hpf_hz,
        });
        cmds.push(LiveCommand::SetNoiseSlicer(o.noise_slicer));
        sent.board = Some(want);
    }
    (cmds, board_level)
}

#[cfg(test)]
mod outputs_tests {
    use super::{SentOutputs, plan_outputs};
    use crate::config::OutputSettings;

    fn run(
        sent: &mut SentOutputs,
        o: &OutputSettings,
        nel: usize,
        af: Option<usize>,
        force: bool,
    ) -> (Vec<String>, bool) {
        let (c, b) = plan_outputs(sent, o, nel, af, force);
        (c.iter().map(|c| format!("{c:?}")).collect(), b)
    }

    fn dac(d: usize, e: Option<usize>, t: f32) -> String {
        format!("DacRoute {{ dac: {d}, electrode: {e:?}, threshold_uv: {t:?} }}")
    }

    fn board(ttl: bool, hpf: bool, hz: f64, ns: i32) -> Vec<String> {
        vec![
            format!("SetTtlMode({ttl})"),
            format!("SetDacHpf {{ enabled: {hpf}, hz: {hz:?} }}"),
            format!("SetNoiseSlicer({ns})"),
        ]
    }

    #[test]
    fn live_commands_for_a_scripted_sequence() {
        let mut sent = SentOutputs::default();
        let mut o = OutputSettings::default();
        let all_default = || {
            let mut v: Vec<String> = (0..8).map(|d| dac(d, None, 0.0)).collect();
            v.extend(board(false, false, 300.0, 0));
            v
        };
        // First start (forced): all 8 routes, then the three board-level commands.
        let (c, b) = run(&mut sent, &o, 32, None, true);
        assert_eq!((c, b), (all_default(), true));
        // Nothing changed: nothing sent.
        assert_eq!(run(&mut sent, &o, 32, None, false), (vec![], false));
        // One DAC route edited: only that route.
        o.dacs[3].electrode = Some(5);
        o.dacs[3].threshold_uv = -80.0;
        assert_eq!(
            run(&mut sent, &o, 32, None, false),
            (vec![dac(3, Some(5), -80.0)], false)
        );
        // The TTL mode edited (the "TTL word" is sent from the panel's buttons, not here):
        // the three board-level commands together, no routes.
        o.ttl_threshold_mode = true;
        assert_eq!(
            run(&mut sent, &o, 32, None, false),
            (board(true, false, 300.0, 0), true)
        );
        // Only the noise slicer edited: still all three.
        o.noise_slicer = 12;
        assert_eq!(
            run(&mut sent, &o, 32, None, false),
            (board(true, false, 300.0, 12), true)
        );
        // DAC 1 follows audio: its route comes from the first audio channel, once.
        o.dac1_follows_audio = true;
        assert_eq!(
            run(&mut sent, &o, 32, Some(7), false),
            (vec![dac(0, Some(7), 0.0)], false)
        );
        assert_eq!(run(&mut sent, &o, 32, Some(7), false), (vec![], false));
        assert_eq!(
            run(&mut sent, &o, 32, Some(9), false),
            (vec![dac(0, Some(9), 0.0)], false)
        );
        // A restart (forced) re-sends everything, whatever was sent before.
        let (c, b) = run(&mut sent, &o, 32, Some(9), true);
        let mut want: Vec<String> = vec![
            dac(0, Some(9), 0.0),
            dac(1, None, 0.0),
            dac(2, None, 0.0),
            dac(3, Some(5), -80.0),
        ];
        want.extend((4..8).map(|d| dac(d, None, 0.0)));
        want.extend(board(true, false, 300.0, 12));
        assert_eq!((c, b), (want, true));
        // The remap drops the route on DAC 4 (electrode None). An electrode beyond the layout
        // counts as off, which DAC 6 already is: nothing sent.
        o.dacs[3].electrode = None;
        o.dacs[5].electrode = Some(40);
        assert_eq!(
            run(&mut sent, &o, 32, Some(9), false),
            (vec![dac(3, None, -80.0)], false)
        );
        // The layout shrinks to 8 electrodes: DAC 1 (audio channel 9) is now beyond it, off.
        assert_eq!(run(&mut sent, &o, 8, Some(9), false), (vec![dac(0, None, 0.0)], false));
        // The high-pass edited: the board-level commands again, with the new values.
        o.dac_hpf_enabled = true;
        o.dac_hpf_hz = 150.0;
        assert_eq!(
            run(&mut sent, &o, 8, Some(9), false),
            (board(true, true, 150.0, 12), true)
        );
    }
}
