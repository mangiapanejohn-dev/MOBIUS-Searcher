//! Threshold panel (`T`): stage changes, review old → new, apply with Enter.
//! Settings under which a landed trade can lose money need the typed
//! acknowledgement `ALLOW LOSS`. Keyboard only, like the kill switch and
//! CONFIRM approvals. The engine validates again, applies, logs and saves.

use crate::hub::ViewModel;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use searcher_core::event::Command;
use searcher_core::thresholds::{THRESHOLDS, parse};
use std::collections::BTreeMap;

pub const ACK: &str = "ALLOW LOSS";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Browse,
    /// Typing a new value for the selected row.
    Edit(String),
    /// Reviewing staged changes; Enter applies.
    Review,
    /// Typing the loss acknowledgement.
    Ack(String),
}

#[derive(Clone, Debug, Default)]
pub struct Panel {
    pub selected: usize,
    pub mode: Mode,
    /// key → new text
    pub staged: BTreeMap<String, String>,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Stay,
    Close(Option<String>),
    Send(Command, String),
}

fn current(vm: &ViewModel, key: &str) -> String {
    vm.thresholds.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default()
}

impl Panel {
    /// The value a key would have after the staged changes.
    pub fn value(&self, vm: &ViewModel, key: &str) -> String {
        self.staged.get(key).cloned().unwrap_or_else(|| current(vm, key))
    }

    /// Staged settings allow a landed trade to lose while the current ones do not.
    pub fn opens_loss(&self, vm: &ViewModel) -> bool {
        let protect_off = self.value(vm, "profit.protect_min_out") == "false";
        let negative = self.value(vm, "profit.min_profit_lamports").parse::<i64>().is_ok_and(|v| v < 0);
        (protect_off || negative) && !vm.loss_possible
    }

    fn send(&self, allow_loss: bool) -> Outcome {
        let changes: Vec<(String, String)> = self.staged.clone().into_iter().collect();
        let n = changes.len();
        Outcome::Send(
            Command::SetThresholds { changes, allow_loss },
            format!("{n} threshold change(s) sent — the engine's answer is in the log"),
        )
    }

    pub fn on_key(&mut self, k: KeyEvent, vm: &ViewModel) -> Outcome {
        let row = &THRESHOLDS[self.selected.min(THRESHOLDS.len() - 1)];
        match &mut self.mode {
            Mode::Edit(buf) => match k.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if buf.len() < 32 => buf.push(c),
                KeyCode::Enter => {
                    let text = buf.trim().to_string();
                    match parse(row, &text) {
                        Ok(_) if text == current(vm, row.key) => {
                            self.staged.remove(row.key);
                            self.error = None;
                            self.mode = Mode::Browse;
                        }
                        Ok(_) => {
                            self.staged.insert(row.key.to_string(), text);
                            self.error = None;
                            self.mode = Mode::Browse;
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                _ => {}
            },
            Mode::Browse => match k.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => {
                    let msg =
                        (!self.staged.is_empty()).then(|| format!("{} staged change(s) discarded", self.staged.len()));
                    return Outcome::Close(msg);
                }
                KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % THRESHOLDS.len(),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = (self.selected + THRESHOLDS.len() - 1) % THRESHOLDS.len()
                }
                KeyCode::Enter | KeyCode::Char('e') => {
                    self.error = None;
                    self.mode = Mode::Edit(self.value(vm, row.key));
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    self.staged.remove(row.key);
                }
                KeyCode::Char('a') if !self.staged.is_empty() => self.mode = Mode::Review,
                _ => {}
            },
            Mode::Review => match k.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Enter if self.opens_loss(vm) => self.mode = Mode::Ack(String::new()),
                KeyCode::Enter => return self.send(false),
                _ => {}
            },
            Mode::Ack(buf) => match k.code {
                KeyCode::Esc => self.mode = Mode::Review,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if buf.len() < ACK.len() + 4 => buf.push(c),
                KeyCode::Enter if buf.as_str() == ACK => return self.send(true),
                KeyCode::Enter => self.error = Some(format!("type {ACK} exactly (Esc to go back)")),
                _ => {}
            },
        }
        Outcome::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn typed(p: &mut Panel, vm: &ViewModel, s: &str) {
        for c in s.chars() {
            assert_eq!(p.on_key(key(KeyCode::Char(c)), vm), Outcome::Stay);
        }
    }

    fn vm() -> ViewModel {
        let mut vm = ViewModel::new(false);
        vm.thresholds = searcher_core::thresholds::values(&searcher_core::config::Config::default());
        vm
    }

    fn select(p: &mut Panel, key: &str) {
        p.selected = THRESHOLDS.iter().position(|t| t.key == key).unwrap();
    }

    fn clear(p: &mut Panel, vm: &ViewModel) {
        for _ in 0..40 {
            p.on_key(key(KeyCode::Backspace), vm);
        }
    }

    #[test]
    fn stage_review_and_apply_a_safe_change() {
        let vm = vm();
        let mut p = Panel::default();
        select(&mut p, "profit.safety_buffer_lamports");
        p.on_key(key(KeyCode::Enter), &vm);
        clear(&mut p, &vm);
        typed(&mut p, &vm, "0");
        p.on_key(key(KeyCode::Enter), &vm);
        assert_eq!(p.staged.get("profit.safety_buffer_lamports").map(String::as_str), Some("0"));
        p.on_key(key(KeyCode::Char('a')), &vm);
        assert_eq!(p.mode, Mode::Review);
        match p.on_key(key(KeyCode::Enter), &vm) {
            Outcome::Send(Command::SetThresholds { changes, allow_loss }, _) => {
                assert_eq!(changes, vec![("profit.safety_buffer_lamports".into(), "0".into())]);
                assert!(!allow_loss);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn turning_protection_off_needs_the_typed_acknowledgement() {
        let vm = vm();
        let mut p = Panel::default();
        select(&mut p, "profit.protect_min_out");
        p.on_key(key(KeyCode::Enter), &vm);
        clear(&mut p, &vm);
        typed(&mut p, &vm, "false");
        p.on_key(key(KeyCode::Enter), &vm);
        p.on_key(key(KeyCode::Char('a')), &vm);
        assert_eq!(p.on_key(key(KeyCode::Enter), &vm), Outcome::Stay, "Enter alone does not apply");
        assert!(matches!(p.mode, Mode::Ack(_)));
        typed(&mut p, &vm, "allow loss");
        assert_eq!(p.on_key(key(KeyCode::Enter), &vm), Outcome::Stay, "exact text only");
        clear(&mut p, &vm);
        typed(&mut p, &vm, ACK);
        match p.on_key(key(KeyCode::Enter), &vm) {
            Outcome::Send(Command::SetThresholds { allow_loss, .. }, _) => assert!(allow_loss),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn invalid_input_is_refused_and_escape_discards() {
        let vm = vm();
        let mut p = Panel::default();
        select(&mut p, "profit.max_new_deposit_lamports");
        p.on_key(key(KeyCode::Enter), &vm);
        clear(&mut p, &vm);
        typed(&mut p, &vm, "-1");
        p.on_key(key(KeyCode::Enter), &vm);
        assert!(p.error.is_some() && p.staged.is_empty());
        p.on_key(key(KeyCode::Esc), &vm);
        select(&mut p, "profit.min_profit_bps");
        p.on_key(key(KeyCode::Enter), &vm);
        clear(&mut p, &vm);
        typed(&mut p, &vm, "0");
        p.on_key(key(KeyCode::Enter), &vm);
        assert_eq!(p.on_key(key(KeyCode::Esc), &vm), Outcome::Close(Some("1 staged change(s) discarded".into())));
    }
}
