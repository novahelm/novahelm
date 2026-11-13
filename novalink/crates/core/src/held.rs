use crate::InputEvent;
use novalink_proto::MouseButton;

/// Tracks keys and buttons currently held down so they can all be released.
#[derive(Debug, Default)]
pub struct HeldInputs {
    keys: Vec<u16>,
    buttons: Vec<MouseButton>,
}

impl HeldInputs {
    pub fn record(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { usage, down: true } => {
                if !self.keys.contains(&usage) {
                    self.keys.push(usage);
                }
            }
            InputEvent::Key { usage, down: false } => self.keys.retain(|&k| k != usage),
            InputEvent::Button { button, down: true } => {
                if !self.buttons.contains(&button) {
                    self.buttons.push(button);
                }
            }
            InputEvent::Button {
                button,
                down: false,
            } => self.buttons.retain(|&b| b != button),
            InputEvent::Motion { .. } | InputEvent::Scroll { .. } => {}
        }
    }

    /// Whether `ev` is a key or button event for an input currently held.
    pub fn contains(&self, ev: &InputEvent) -> bool {
        match *ev {
            InputEvent::Key { usage, .. } => self.keys.contains(&usage),
            InputEvent::Button { button, .. } => self.buttons.contains(&button),
            InputEvent::Motion { .. } | InputEvent::Scroll { .. } => false,
        }
    }

    /// Key-ups for held keys, then button-ups for held buttons; leaves the set empty.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let keys = self
            .keys
            .drain(..)
            .map(|usage| InputEvent::Key { usage, down: false });
        let buttons = self.buttons.drain(..).map(|button| InputEvent::Button {
            button,
            down: false,
        });
        keys.chain(buttons).collect()
    }

    /// Number of keys and buttons currently held.
    pub fn len(&self) -> usize {
        self.keys.len() + self.buttons.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novalink_proto::keys;

    #[test]
    fn contains_matches_held_key_or_button_in_either_direction() {
        let mut h = HeldInputs::default();
        h.record(&InputEvent::Key {
            usage: keys::A,
            down: true,
        });
        h.record(&InputEvent::Button {
            button: MouseButton::Left,
            down: true,
        });
        for down in [true, false] {
            assert!(h.contains(&InputEvent::Key {
                usage: keys::A,
                down
            }));
            assert!(h.contains(&InputEvent::Button {
                button: MouseButton::Left,
                down
            }));
        }
        assert!(!h.contains(&InputEvent::Key {
            usage: keys::B,
            down: true
        }));
        assert!(!h.contains(&InputEvent::Motion { dx: 1.0, dy: 0.0 }));
    }

    #[test]
    fn release_all_ups_everything_once() {
        let mut h = HeldInputs::default();
        h.record(&InputEvent::Key {
            usage: keys::LEFT_SHIFT,
            down: true,
        });
        h.record(&InputEvent::Key {
            usage: keys::A,
            down: true,
        });
        h.record(&InputEvent::Key {
            usage: keys::A,
            down: false,
        });
        h.record(&InputEvent::Button {
            button: MouseButton::Left,
            down: true,
        });
        assert_eq!(
            h.release_all(),
            vec![
                InputEvent::Key {
                    usage: keys::LEFT_SHIFT,
                    down: false
                },
                InputEvent::Button {
                    button: MouseButton::Left,
                    down: false
                }
            ]
        );
        assert!(h.is_empty() && h.release_all().is_empty());
    }

    #[test]
    fn repeated_down_is_recorded_once() {
        let mut h = HeldInputs::default();
        h.record(&InputEvent::Key {
            usage: keys::A,
            down: true,
        });
        h.record(&InputEvent::Key {
            usage: keys::A,
            down: true,
        });
        assert_eq!(h.release_all().len(), 1);
    }
}
