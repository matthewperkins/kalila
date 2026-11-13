//! Typed, sample-stamped notes (Open Ephys MessageCenter text events in the recording).
//!
//! Works in any kalila window while no text box has focus; never needs the mouse.
//! - `/` opens a note stamped at that sample. Every key then goes into the note, so no
//!   shortcut fires. Enter or `\` saves it, Esc cancels it.
//! - `` ` `` stamps the next prepared draft (drafts are written ahead in the Notes panel).
//! - 1–9 stamp the preset on that key.
//!
//! Every keypress of a typed note is kept, sample-stamped, for `note_keystrokes.tsv`.

use crate::config::NotesSettings;
use eframe::egui::{self, Event, Key};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub sample: i64,
    pub text: String,
}

/// A note being typed.
#[derive(Debug, Default)]
pub struct Compose {
    /// Sample of the `/` that opened it (the note's stamp).
    pub sample: i64,
    pub text: String,
    /// (sample, key) of every keypress, including the opening `/` and the closing key.
    pub keys: Vec<(i64, String)>,
}

/// A finished note, for the recorder and the notes log.
#[derive(Debug, PartialEq)]
pub struct Finished {
    pub note: Note,
    pub keys: Vec<(i64, String)>,
}

#[derive(Default)]
pub struct Notes {
    pub compose: Option<Compose>,
    pub drafts: VecDeque<String>,
    /// Presets on keys 1–9 (empty = none).
    pub presets: Vec<String>,
    /// This acquisition's notes (markers on the traces).
    pub notes: Vec<Note>,
    /// Finished notes not yet sent to the recorder and the log.
    pub out: Vec<Finished>,
    /// A short confirmation or explanation, until the instant.
    pub flash: Option<(Instant, String)>,
    /// Drafts or presets changed (save the config).
    pub changed: bool,
}

const PRESET_KEYS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

impl Notes {
    pub fn new(s: &NotesSettings) -> Self {
        let mut presets = s.presets.clone();
        presets.resize(9, String::new());
        Self { drafts: s.drafts.iter().cloned().collect(), presets, ..Default::default() }
    }

    pub fn settings(&self) -> NotesSettings {
        NotesSettings { presets: self.presets.clone(), drafts: self.drafts.iter().cloned().collect() }
    }

    fn say(&mut self, msg: String) {
        self.flash = Some((Instant::now() + Duration::from_secs(3), msg));
    }

    fn finish(&mut self, note: Note, keys: Vec<(i64, String)>) {
        self.say(format!("Note: {}", note.text));
        self.notes.push(note.clone());
        self.out.push(Finished { note, keys });
    }

    /// Handle one window's input for this frame. `typing` = a text box there has focus;
    /// `now` = the newest sample, or None when not acquiring. Events used for notes are
    /// removed, so nothing else in the window sees them.
    pub fn handle(&mut self, events: &mut Vec<Event>, typing: bool, now: Option<i64>) {
        if let Some(c) = self.compose.as_mut() {
            let k = now.unwrap_or(c.sample);
            let mut done = None;
            for e in events.iter() {
                if done.is_some() {
                    break;
                }
                match e {
                    Event::Text(t) => {
                        for ch in t.chars() {
                            c.keys.push((k, ch.to_string()));
                            if ch == '\\' {
                                done = Some(true);
                                break;
                            }
                            c.text.push(ch);
                        }
                    }
                    Event::Key { key, pressed: true, .. } => match key {
                        Key::Backspace => {
                            c.keys.push((k, "<BS>".into()));
                            c.text.pop();
                        }
                        Key::Enter => {
                            c.keys.push((k, "<Enter>".into()));
                            done = Some(true);
                        }
                        Key::Escape => {
                            c.keys.push((k, "<Esc>".into()));
                            done = Some(false);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            // While a note is open, all keys belong to it.
            events.retain(|e| !matches!(e, Event::Text(_) | Event::Key { .. } | Event::Paste(_)));
            match done {
                Some(true) => {
                    let c = self.compose.take().unwrap();
                    let text = c.text.trim().to_string();
                    if text.is_empty() {
                        self.say("Empty note discarded".into());
                    } else {
                        self.finish(Note { sample: c.sample, text }, c.keys);
                    }
                }
                Some(false) => {
                    self.compose = None;
                    self.say("Note cancelled".into());
                }
                None => {}
            }
            return;
        }
        if typing {
            return;
        }
        let mut used: Option<Key> = None;
        let mut idx = None;
        for (i, e) in events.iter().enumerate() {
            let Event::Text(t) = e else { continue };
            let key = match t.as_str() {
                "/" => Key::Slash,
                "`" => Key::Backtick,
                d if d.len() == 1 && ('1'..='9').contains(&d.chars().next().unwrap()) => PRESET_KEYS[(d.as_bytes()[0] - b'1') as usize],
                _ => continue,
            };
            used = Some(key);
            idx = Some(i);
            break;
        }
        let (Some(key), Some(i)) = (used, idx) else { return };
        let Some(s) = now else {
            self.say("Notes need acquisition running".into());
            return;
        };
        match key {
            Key::Slash => self.compose = Some(Compose { sample: s, text: String::new(), keys: vec![(s, "/".into())] }),
            Key::Backtick => match self.drafts.pop_front() {
                Some(d) => {
                    self.changed = true;
                    self.finish(Note { sample: s, text: d }, vec![]);
                }
                None => self.say("No drafts left (add them in the Notes panel)".into()),
            },
            _ => {
                let n = PRESET_KEYS.iter().position(|k| *k == key).unwrap();
                match self.presets.get(n).filter(|p| !p.trim().is_empty()).cloned() {
                    Some(p) => self.finish(Note { sample: s, text: p.trim().to_string() }, vec![]),
                    None => self.say(format!("No preset on {} (set one in the Notes panel)", n + 1)),
                }
            }
        }
        events.remove(i);
        events.retain(|e| !matches!(e, Event::Key { key: k, .. } if *k == key));
    }

    /// Bar at the bottom of a window while typing, or a short confirmation.
    pub fn draw(&mut self, ctx: &egui::Context, viewport: egui::ViewportId, time_of: impl Fn(i64) -> String) {
        let text = if let Some(c) = &self.compose {
            Some((format!("Note {}   {}|", time_of(c.sample), c.text), "Enter or \\ saves · Esc cancels".to_string(), true))
        } else if let Some((until, msg)) = &self.flash {
            if Instant::now() < *until { Some((msg.clone(), String::new(), false)) } else { None }
        } else {
            None
        };
        let Some((main, hint, open)) = text else {
            self.flash = None;
            return;
        };
        egui::Area::new(egui::Id::unique(("note-bar", viewport))).anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -28.0)).order(egui::Order::Foreground).interactable(false).show(ctx, |ui| {
            let fill = if open { egui::Color32::from_rgb(70, 55, 15) } else { egui::Color32::from_gray(40) };
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            egui::Frame::popup(ui.style()).fill(fill).inner_margin(10.0).show(ui, |ui| {
                ui.label(egui::RichText::new(main).size(17.0).color(egui::Color32::from_rgb(255, 220, 140)));
                if !hint.is_empty() {
                    ui.small(hint);
                }
            });
        });
        ctx.request_repaint_of(viewport);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Event {
        Event::Text(s.into())
    }
    fn key(k: Key) -> Event {
        Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }
    }

    #[test]
    fn typed_note_is_stamped_at_the_slash_with_every_key() {
        let mut n = Notes::new(&NotesSettings::default());
        let mut ev = vec![text("/"), key(Key::Slash)];
        n.handle(&mut ev, false, Some(1000));
        assert!(ev.is_empty() && n.compose.is_some());
        let mut ev = vec![text("dx"), key(Key::ArrowLeft)];
        n.handle(&mut ev, false, Some(1300));
        assert!(ev.is_empty(), "keys go to the note, not to shortcuts");
        n.handle(&mut vec![key(Key::Backspace)], false, Some(1400));
        n.handle(&mut vec![text("rink")], false, Some(1500));
        n.handle(&mut vec![text("\\")], false, Some(1600));
        assert!(n.compose.is_none());
        let f = &n.out[0];
        assert_eq!(f.note, Note { sample: 1000, text: "drink".into() });
        assert_eq!(f.keys.first(), Some(&(1000, "/".to_string())));
        assert!(f.keys.contains(&(1400, "<BS>".to_string())));
        assert_eq!(f.keys.last(), Some(&(1600, "\\".to_string())));
    }

    #[test]
    fn escape_cancels_and_enter_saves() {
        let mut n = Notes::new(&NotesSettings::default());
        n.handle(&mut vec![text("/")], false, Some(5));
        n.handle(&mut vec![text("oops"), key(Key::Escape)], false, Some(6));
        assert!(n.compose.is_none() && n.out.is_empty());
        n.handle(&mut vec![text("/")], false, Some(7));
        n.handle(&mut vec![text("eating"), key(Key::Enter)], false, Some(8));
        assert_eq!(n.out[0].note, Note { sample: 7, text: "eating".into() });
    }

    #[test]
    fn drafts_and_presets_stamp_with_one_key() {
        let mut n = Notes::new(&NotesSettings { presets: vec!["drinking".into(), "".into()], drafts: vec!["laser 2 mW".into(), "laser 4 mW".into()] });
        let mut ev = vec![text("`"), key(Key::Backtick)];
        n.handle(&mut ev, false, Some(100));
        assert!(ev.is_empty());
        n.handle(&mut vec![text("1")], false, Some(200));
        n.handle(&mut vec![text("2")], false, Some(300)); // empty preset: nothing
        let got: Vec<_> = n.out.iter().map(|f| (f.note.sample, f.note.text.as_str())).collect();
        assert_eq!(got, vec![(100, "laser 2 mW"), (200, "drinking")]);
        assert_eq!(n.drafts, vec!["laser 4 mW".to_string()]);
    }

    #[test]
    fn nothing_happens_while_a_text_box_has_focus_or_without_acquisition() {
        let mut n = Notes::new(&NotesSettings { presets: vec!["x".into()], drafts: vec![] });
        let mut ev = vec![text("/"), text("1")];
        n.handle(&mut ev, true, Some(1));
        assert_eq!(ev.len(), 2);
        assert!(n.compose.is_none());
        n.handle(&mut vec![text("/")], false, None);
        assert!(n.compose.is_none() && n.flash.is_some());
    }
}
