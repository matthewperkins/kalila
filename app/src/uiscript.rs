//! Scripted GUI input for testing without a real pointer: `OERS_UI_SCRIPT=<file>`.
//!
//! The steps are injected through `App::raw_input_hook` into one window's own egui input. The
//! system pointer and keyboard and other programs are never touched. Off unless the variable is
//! set.
//!
//! One step per line; `#` starts a comment. Coordinates are points inside the target window
//! (on the rig, points = pixels). Modifier words (`shift`, `ctrl`, `alt`) can follow any
//! pointer or key step; `right` / `middle` pick the button.
//!
//! ```text
//! window Port B          # later steps go to the window whose title contains this
//! wait 1.5               # seconds
//! move 400 300
//! click 520 12 [right] [shift]
//! drag 600 400 300 400 [steps]
//! scroll 600 400 0 -120 [alt]    # dx dy in points (negative dy = wheel down)
//! key ArrowLeft [shift]          # egui key names: Enter, Escape, Home, End, A, Minus, …
//! text hello
//! shot palette-open      # PNG of the target window into OERS_UI_SHOTS (default: script dir)
//! log any message
//! quit                   # ask the main window to close
//! ```

use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2, Vec2, ViewportCommand, ViewportId};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Window(String),
    Wait(f64),
    Move(Pos2),
    Click { pos: Pos2, button: PointerButton, mods: Modifiers },
    Drag { from: Pos2, to: Pos2, button: PointerButton, mods: Modifiers, steps: u32 },
    Scroll { pos: Pos2, delta: Vec2, mods: Modifiers },
    Key { key: egui::Key, mods: Modifiers },
    Text(String),
    Shot(String),
    Log(String),
    Quit,
}

/// Events for one frame of the target window, with the modifiers held during it.
struct Batch {
    events: Vec<Event>,
    mods: Modifiers,
}

pub struct UiScript {
    steps: VecDeque<Step>,
    /// Title substring of the window that receives input.
    target: String,
    batches: VecDeque<Batch>,
    wait_until: Option<Instant>,
    shots: PathBuf,
    done: bool,
}

fn parse_line(line: &str) -> Result<Option<Step>, String> {
    let line = line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
        return Ok(None);
    }
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let words: Vec<&str> = rest.split_whitespace().collect();
    let mut mods = Modifiers::NONE;
    let mut button = PointerButton::Primary;
    let mut nums: Vec<f32> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for w in &words {
        match *w {
            "shift" => mods.shift = true,
            "ctrl" => {
                mods.ctrl = true;
                mods.command = true;
            }
            "alt" => mods.alt = true,
            "right" => button = PointerButton::Secondary,
            "middle" => button = PointerButton::Middle,
            _ => match w.parse::<f32>() {
                Ok(v) => nums.push(v),
                Err(_) => other.push(w),
            },
        }
    }
    let need = |n: usize| if nums.len() >= n { Ok(()) } else { Err(format!("`{cmd}` needs {n} numbers: {line}")) };
    let p = |i: usize| Pos2::new(nums[i], nums[i + 1]);
    Ok(Some(match cmd {
        "window" => Step::Window(rest.to_string()),
        "wait" => {
            need(1)?;
            Step::Wait(nums[0] as f64)
        }
        "move" => {
            need(2)?;
            Step::Move(p(0))
        }
        "click" => {
            need(2)?;
            Step::Click { pos: p(0), button, mods }
        }
        "drag" => {
            need(4)?;
            Step::Drag { from: p(0), to: p(2), button, mods, steps: nums.get(4).map_or(12, |&n| n.max(1.0) as u32) }
        }
        "scroll" => {
            need(4)?;
            Step::Scroll { pos: p(0), delta: Vec2::new(nums[2], nums[3]), mods }
        }
        "key" => {
            let name = other.first().ok_or(format!("`key` needs a key name: {line}"))?;
            Step::Key { key: egui::Key::from_name(name).ok_or(format!("unknown key `{name}`"))?, mods }
        }
        "text" => Step::Text(rest.to_string()),
        "shot" => Step::Shot(if rest.is_empty() { "shot".into() } else { rest.to_string() }),
        "log" => Step::Log(rest.to_string()),
        "quit" => Step::Quit,
        _ => return Err(format!("unknown step `{cmd}`")),
    }))
}

fn parse(src: &str) -> Result<VecDeque<Step>, String> {
    let mut v = VecDeque::new();
    for (i, line) in src.lines().enumerate() {
        if let Some(s) = parse_line(line).map_err(|e| format!("line {}: {e}", i + 1))? {
            v.push_back(s);
        }
    }
    Ok(v)
}

fn save_png(path: &Path, img: &egui::ColorImage) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    w.write_image_data(&bytes).map_err(|e| e.to_string())
}

impl UiScript {
    /// The script named by `OERS_UI_SCRIPT`, if set (errors are logged and disable it).
    pub fn from_env() -> Option<Self> {
        let path = PathBuf::from(std::env::var_os("OERS_UI_SCRIPT")?);
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                log::error!("ui-script: can't read {}: {e}", path.display());
                return None;
            }
        };
        let steps = match parse(&src) {
            Ok(s) => s,
            Err(e) => {
                log::error!("ui-script {}: {e}", path.display());
                return None;
            }
        };
        let shots = std::env::var_os("OERS_UI_SHOTS").map(PathBuf::from).unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());
        log::warn!("ui-script: running {} ({} steps); scripted input goes to oers windows only", path.display(), steps.len());
        Some(Self { steps, target: "Open Ephys acquisition".into(), batches: VecDeque::new(), wait_until: None, shots, done: false })
    }

    /// Called for every window's input each frame.
    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if self.done {
            return;
        }
        let target = raw.viewports.iter().find(|(_, v)| v.title.as_deref().is_some_and(|t| t.contains(&self.target))).map(|(id, _)| *id);
        let Some(target) = target else {
            // Not open (yet): keep the main window ticking until it is.
            ctx.request_repaint_of(ViewportId::ROOT);
            return;
        };
        ctx.request_repaint_of(target);
        if raw.viewport_id != target {
            return;
        }
        if let Some(b) = self.batches.pop_front() {
            raw.events.push(Event::ModifiersChanged(b.mods));
            raw.events.extend(b.events);
            raw.focused = true;
            return;
        }
        if self.wait_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.wait_until = None;
        while let Some(step) = self.steps.pop_front() {
            log::info!("ui-script: {step:?}");
            let one = |events: Vec<Event>, mods: Modifiers| Batch { events, mods };
            match step {
                Step::Log(m) => {
                    log::info!("ui-script: {m}");
                    continue;
                }
                Step::Window(t) => self.target = t,
                Step::Wait(s) => self.wait_until = Some(Instant::now() + Duration::from_secs_f64(s.max(0.0))),
                Step::Move(p) => self.batches.push_back(one(vec![Event::PointerMoved(p)], Modifiers::NONE)),
                Step::Click { pos, button, mods } => {
                    self.batches.push_back(one(vec![Event::PointerMoved(pos)], mods));
                    self.batches.push_back(one(vec![Event::PointerButton { pos, button, pressed: true, modifiers: mods }], mods));
                    self.batches.push_back(one(vec![Event::PointerButton { pos, button, pressed: false, modifiers: mods }], mods));
                }
                Step::Drag { from, to, button, mods, steps } => {
                    self.batches.push_back(one(vec![Event::PointerMoved(from)], mods));
                    self.batches.push_back(one(vec![Event::PointerButton { pos: from, button, pressed: true, modifiers: mods }], mods));
                    for i in 1..=steps {
                        self.batches.push_back(one(vec![Event::PointerMoved(from.lerp(to, i as f32 / steps as f32))], mods));
                    }
                    self.batches.push_back(one(vec![Event::PointerButton { pos: to, button, pressed: false, modifiers: mods }], mods));
                }
                Step::Scroll { pos, delta, mods } => {
                    self.batches.push_back(one(vec![Event::PointerMoved(pos)], mods));
                    self.batches.push_back(one(
                        vec![Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Wheel, modifiers: mods }],
                        mods,
                    ));
                    // A few idle frames, so the smoothed scroll lands while the modifiers are held.
                    for _ in 0..8 {
                        self.batches.push_back(one(vec![], mods));
                    }
                }
                Step::Key { key, mods } => {
                    self.batches.push_back(one(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: mods }], mods));
                    self.batches.push_back(one(vec![Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: mods }], mods));
                }
                Step::Text(t) => self.batches.push_back(one(vec![Event::Text(t)], Modifiers::NONE)),
                Step::Shot(name) => {
                    let path = self.shots.join(format!("{name}.png"));
                    let cb = egui::ScreenshotCallback::new(move |img| match save_png(&path, &img) {
                        Ok(()) => log::info!("ui-script: saved {}", path.display()),
                        Err(e) => log::error!("ui-script: saving {}: {e}", path.display()),
                    });
                    ctx.send_viewport_cmd_to(target, ViewportCommand::Screenshot(cb));
                    // Let the screenshot frame render before the next step changes anything.
                    self.wait_until = Some(Instant::now() + Duration::from_millis(300));
                }
                Step::Quit => ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close),
            }
            // Input steps end with the modifiers released.
            if !self.batches.is_empty() {
                self.batches.push_back(Batch { events: vec![], mods: Modifiers::NONE });
            }
            break;
        }
        if self.steps.is_empty() && self.batches.is_empty() && self.wait_until.is_none() {
            log::info!("ui-script: done");
            self.done = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps_modifiers_and_comments() {
        let s = parse("# setup\nwindow Port B\nwait 0.5\nclick 10 20 right shift  # palette\ndrag 1 2 3 4 6\nscroll 5 6 0 -120 alt\nkey ArrowLeft shift\nshot a b\nquit\n").unwrap();
        assert_eq!(s[0], Step::Window("Port B".into()));
        assert_eq!(s[1], Step::Wait(0.5));
        assert_eq!(s[2], Step::Click { pos: Pos2::new(10.0, 20.0), button: PointerButton::Secondary, mods: Modifiers::SHIFT });
        assert_eq!(s[3], Step::Drag { from: Pos2::new(1.0, 2.0), to: Pos2::new(3.0, 4.0), button: PointerButton::Primary, mods: Modifiers::NONE, steps: 6 });
        assert_eq!(s[4], Step::Scroll { pos: Pos2::new(5.0, 6.0), delta: Vec2::new(0.0, -120.0), mods: Modifiers::ALT });
        assert_eq!(s[5], Step::Key { key: egui::Key::ArrowLeft, mods: Modifiers::SHIFT });
        assert_eq!(s[6], Step::Shot("a b".into()));
        assert_eq!(s[7], Step::Quit);
        assert!(parse("click 1").is_err());
        assert!(parse("key Nope").is_err());
        assert!(parse("jump 1 2").is_err());
    }
}
