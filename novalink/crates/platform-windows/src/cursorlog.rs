//! Watches the pointer while another machine moves it, for the log. An app can hide the
//! pointer (many do after a few idle seconds) and, unlike with a real mouse, may not show
//! it again when NovaLink moves it. The log says when it was hidden, over which app, and
//! how many moves it then missed. The decisions are pure so they are tested anywhere.

use std::time::Duration;

pub const SAMPLE_EVERY: Duration = Duration::from_millis(250);
/// Hide/show changes logged per visit before the log goes quiet, so an app that flickers
/// the pointer can't flood it.
pub const MAX_LOGGED: u32 = 20;
/// `CURSORINFO.flags`: the pointer is drawn.
pub const CURSOR_SHOWING: u32 = 0x1;

#[derive(Debug, PartialEq, Eq)]
pub enum Note {
    /// The pointer stopped showing. `flags` is 0 when an app hid it and 2 when Windows
    /// held it back (after touch or pen input).
    Hidden { flags: u32 },
    /// It shows again after `hidden_ms`, during which NovaLink sent `moves` moves.
    Shown { hidden_ms: u64, moves: u64 },
    /// `MAX_LOGGED` reached; nothing more this visit.
    Quiet,
}

/// One visit of the remote pointer.
#[derive(Default)]
pub struct Watch {
    /// When it was hidden (ms) and the move count then.
    hidden_since: Option<(u64, u64)>,
    logged: u32,
}

impl Watch {
    /// A sample at `now_ms` with the cursor's `flags` and the moves sent so far.
    pub fn sample(&mut self, now_ms: u64, flags: u32, moves: u64) -> Option<Note> {
        let showing = flags & CURSOR_SHOWING != 0;
        let note = match (self.hidden_since, showing) {
            (None, false) => {
                self.hidden_since = Some((now_ms, moves));
                Note::Hidden { flags }
            }
            (Some((since, at)), true) => {
                self.hidden_since = None;
                Note::Shown {
                    hidden_ms: now_ms.saturating_sub(since),
                    moves: moves.saturating_sub(at),
                }
            }
            _ => return None,
        };
        self.logged += 1;
        match self.logged {
            n if n <= MAX_LOGGED => Some(note),
            n if n == MAX_LOGGED + 1 => Some(Note::Quiet),
            _ => None,
        }
    }

    /// When the visit ends: how long it had been hidden and the moves it missed, if it
    /// still was. Logged even when quiet: it is the line that matters most.
    pub fn end(&self, now_ms: u64, moves: u64) -> Option<(u64, u64)> {
        self.hidden_since
            .map(|(since, at)| (now_ms.saturating_sub(since), moves.saturating_sub(at)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_each_hide_and_show_with_the_moves_in_between() {
        let mut w = Watch::default();
        assert_eq!(w.sample(0, 1, 0), None);
        assert_eq!(w.sample(250, 0, 10), Some(Note::Hidden { flags: 0 }));
        assert_eq!(w.sample(500, 0, 30), None);
        assert_eq!(
            w.sample(750, 1, 42),
            Some(Note::Shown {
                hidden_ms: 500,
                moves: 32
            })
        );
        assert_eq!(w.end(1000, 50), None);
        assert_eq!(w.sample(1000, 2, 50), Some(Note::Hidden { flags: 2 }));
        assert_eq!(w.end(4000, 90), Some((3000, 40)));
    }

    #[test]
    fn goes_quiet_after_the_cap_but_still_reports_the_end() {
        let mut w = Watch::default();
        let mut notes = Vec::new();
        for i in 0..(2 * MAX_LOGGED as u64 + 10) {
            notes.extend(w.sample(i * 250, (i % 2) as u32, i));
        }
        assert_eq!(notes.len(), MAX_LOGGED as usize + 1);
        assert_eq!(notes.last(), Some(&Note::Quiet));
        assert_eq!(w.sample(100_000, 0, 500), None);
        assert!(w.end(101_000, 600).is_some());
    }
}
