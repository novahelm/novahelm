//! Platform abstraction (capture / inject / displays) and a test double.

use crate::input::{InputEvent, LocalEvent};
use novalink_proto::DisplayInfo;
use tokio::sync::mpsc::UnboundedSender;

pub trait Platform: Send + Sync + 'static {
    fn displays(&self) -> Vec<DisplayInfo>;
    fn start_capture(&self, tx: UnboundedSender<LocalEvent>) -> anyhow::Result<()>;
    /// Swallow local physical input; freeze + hide the cursor.
    fn set_suppress(&self, on: bool);
    fn inject(&self, ev: &InputEvent) -> anyhow::Result<()>;
    fn warp(&self, pos: (f64, f64));
    fn cursor_pos(&self) -> (f64, f64);
    /// Permission name + granted flag, for `doctor`.
    fn permissions(&self) -> Vec<(&'static str, bool)>;
    /// Platform self-healing events since the last call (`tap_reenabled`,
    /// `hook_reinstalled`). Each is reported once.
    fn take_health_events(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// Ask the OS to show its permission prompts and list this process in the privacy
    /// settings. Does nothing where no permission is needed.
    fn request_permissions(&self) {}
}

#[cfg(feature = "fake")]
pub use fake::FakePlatform;

#[cfg(feature = "fake")]
mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct State {
        displays: Vec<DisplayInfo>,
        perms: Vec<(&'static str, bool)>,
        cursor: (f64, f64),
        tx: Option<UnboundedSender<LocalEvent>>,
        injected: Vec<InputEvent>,
        held: Vec<u16>,
        last_warp: Option<(f64, f64)>,
        suppressed: bool,
        health: Vec<&'static str>,
        deny_capture: bool,
        requests: u32,
    }

    /// In-memory `Platform` for tests. Clones share state.
    #[derive(Clone)]
    pub struct FakePlatform {
        st: Arc<Mutex<State>>,
    }

    impl Default for FakePlatform {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakePlatform {
        /// One 1920x1080 display at 0,0; all permissions granted.
        pub fn new() -> Self {
            Self {
                st: Arc::new(Mutex::new(State {
                    displays: vec![DisplayInfo {
                        id: 1,
                        x: 0.0,
                        y: 0.0,
                        width: 1920.0,
                        height: 1080.0,
                        scale: 1.0,
                    }],
                    perms: vec![("accessibility", true), ("input_monitoring", true)],
                    cursor: (0.0, 0.0),
                    tx: None,
                    injected: Vec::new(),
                    held: Vec::new(),
                    last_warp: None,
                    suppressed: false,
                    health: Vec::new(),
                    deny_capture: false,
                    requests: 0,
                })),
            }
        }

        fn emit(&self, st: &State, event: InputEvent, physical: bool) {
            if let Some(tx) = &st.tx {
                let _ = tx.send(LocalEvent {
                    event,
                    physical,
                    t_us: crate::time::mono_us(),
                    pos: st.cursor,
                });
            }
        }

        /// Hardware moved the cursor to `pos`: sets the cursor, emits physical motion.
        pub fn push_physical_motion_to(&self, pos: (f64, f64)) {
            let mut st = self.st.lock().unwrap();
            let (dx, dy) = ((pos.0 - st.cursor.0) as f32, (pos.1 - st.cursor.1) as f32);
            st.cursor = pos;
            self.emit(&st, InputEvent::Motion { dx, dy }, true);
        }

        /// Hardware moved by a delta. The cursor follows unless suppressed (frozen).
        pub fn push_physical_motion_delta(&self, dx: f32, dy: f32) {
            let mut st = self.st.lock().unwrap();
            if !st.suppressed {
                st.cursor = (st.cursor.0 + dx as f64, st.cursor.1 + dy as f64);
            }
            self.emit(&st, InputEvent::Motion { dx, dy }, true);
        }

        pub fn push_physical_key(&self, usage: u16, down: bool) {
            let st = self.st.lock().unwrap();
            self.emit(&st, InputEvent::Key { usage, down }, true);
        }

        pub fn push_physical_button(&self, button: novalink_proto::MouseButton, down: bool) {
            let st = self.st.lock().unwrap();
            self.emit(&st, InputEvent::Button { button, down }, true);
        }

        /// Sets the cursor and emits non-physical motion, as if our own injection moved it.
        pub fn push_injected_motion_to(&self, pos: (f64, f64)) {
            let mut st = self.st.lock().unwrap();
            let (dx, dy) = ((pos.0 - st.cursor.0) as f32, (pos.1 - st.cursor.1) as f32);
            st.cursor = pos;
            self.emit(&st, InputEvent::Motion { dx, dy }, false);
        }

        pub fn set_displays(&self, d: Vec<DisplayInfo>) {
            self.st.lock().unwrap().displays = d;
        }

        pub fn set_permissions(&self, p: Vec<(&'static str, bool)>) {
            self.st.lock().unwrap().perms = p;
        }

        pub fn injected_contains(&self, ev: &InputEvent) -> bool {
            self.st.lock().unwrap().injected.contains(ev)
        }

        /// Usages currently down according to injected Key events.
        pub fn held_now(&self) -> Vec<u16> {
            self.st.lock().unwrap().held.clone()
        }

        pub fn last_warp(&self) -> Option<(f64, f64)> {
            self.st.lock().unwrap().last_warp
        }

        /// Queues a health event for the next `take_health_events`.
        pub fn push_health_event(&self, ev: &'static str) {
            self.st.lock().unwrap().health.push(ev);
        }

        /// While set, `start_capture` fails and the permissions read as not granted,
        /// as on a Mac without Accessibility or Input Monitoring.
        pub fn deny_capture(&self, on: bool) {
            self.st.lock().unwrap().deny_capture = on;
        }

        /// How many times `request_permissions` was called.
        pub fn requests(&self) -> u32 {
            self.st.lock().unwrap().requests
        }

        pub fn suppressed(&self) -> bool {
            self.st.lock().unwrap().suppressed
        }
    }

    impl Platform for FakePlatform {
        fn displays(&self) -> Vec<DisplayInfo> {
            self.st.lock().unwrap().displays.clone()
        }

        fn start_capture(&self, tx: UnboundedSender<LocalEvent>) -> anyhow::Result<()> {
            let mut st = self.st.lock().unwrap();
            anyhow::ensure!(!st.deny_capture, "input capture is not permitted");
            anyhow::ensure!(st.tx.is_none(), "capture already started");
            st.tx = Some(tx);
            Ok(())
        }

        fn set_suppress(&self, on: bool) {
            self.st.lock().unwrap().suppressed = on;
        }

        /// Records the event. Injected motion does not move the fake cursor;
        /// tests use `push_injected_motion_to` for that.
        fn inject(&self, ev: &InputEvent) -> anyhow::Result<()> {
            let mut st = self.st.lock().unwrap();
            if let InputEvent::Key { usage, down } = *ev {
                if down {
                    if !st.held.contains(&usage) {
                        st.held.push(usage);
                    }
                } else {
                    st.held.retain(|&u| u != usage);
                }
            }
            st.injected.push(*ev);
            Ok(())
        }

        fn warp(&self, pos: (f64, f64)) {
            let mut st = self.st.lock().unwrap();
            st.cursor = pos;
            st.last_warp = Some(pos);
        }

        fn cursor_pos(&self) -> (f64, f64) {
            self.st.lock().unwrap().cursor
        }

        fn permissions(&self) -> Vec<(&'static str, bool)> {
            let st = self.st.lock().unwrap();
            let mut perms = st.perms.clone();
            if st.deny_capture {
                perms.iter_mut().for_each(|p| p.1 = false);
            }
            perms
        }

        fn request_permissions(&self) {
            self.st.lock().unwrap().requests += 1;
        }

        fn take_health_events(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.st.lock().unwrap().health)
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod tests {
    use super::*;
    use novalink_proto::{MouseButton, keys};
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn defaults() {
        let p = FakePlatform::new();
        let d = p.displays();
        assert_eq!(d.len(), 1);
        assert_eq!(
            (d[0].x, d[0].y, d[0].width, d[0].height),
            (0.0, 0.0, 1920.0, 1080.0)
        );
        assert!(p.permissions().iter().all(|&(_, ok)| ok));
        assert!(!p.suppressed());
        assert_eq!(p.last_warp(), None);
        assert!(p.held_now().is_empty());
    }

    #[test]
    fn start_capture_twice_errors_and_push_without_capture_is_noop() {
        let p = FakePlatform::new();
        p.push_physical_key(keys::A, true); // no capture yet: must not panic
        let (tx, _rx) = unbounded_channel();
        p.start_capture(tx.clone()).unwrap();
        assert!(p.start_capture(tx).is_err());
    }

    #[test]
    fn physical_motion_to_sets_cursor_and_emits_delta() {
        let p = FakePlatform::new();
        let (tx, mut rx) = unbounded_channel();
        p.start_capture(tx).unwrap();
        p.warp((100.0, 100.0));
        p.push_physical_motion_to((110.0, 95.0));
        assert_eq!(p.cursor_pos(), (110.0, 95.0));
        let e = rx.try_recv().unwrap();
        assert!(e.physical);
        assert_eq!(e.pos, (110.0, 95.0));
        assert_eq!(e.event, InputEvent::Motion { dx: 10.0, dy: -5.0 });
    }

    #[test]
    fn physical_delta_and_key() {
        let p = FakePlatform::new();
        let (tx, mut rx) = unbounded_channel();
        p.start_capture(tx).unwrap();
        p.warp((50.0, 50.0));
        p.push_physical_motion_delta(3.0, 4.0);
        let e = rx.try_recv().unwrap();
        assert!(e.physical);
        assert_eq!(e.event, InputEvent::Motion { dx: 3.0, dy: 4.0 });
        assert_eq!(p.cursor_pos(), (53.0, 54.0));
        p.push_physical_key(keys::A, true);
        let e = rx.try_recv().unwrap();
        assert!(e.physical);
        assert_eq!(
            e.event,
            InputEvent::Key {
                usage: keys::A,
                down: true
            }
        );
    }

    #[test]
    fn suppress_freezes_cursor_on_delta_but_still_reports() {
        let p = FakePlatform::new();
        let (tx, mut rx) = unbounded_channel();
        p.start_capture(tx).unwrap();
        p.warp((50.0, 50.0));
        p.set_suppress(true);
        assert!(p.suppressed());
        p.push_physical_motion_delta(3.0, 4.0);
        assert_eq!(p.cursor_pos(), (50.0, 50.0));
        assert!(rx.try_recv().is_ok());
        p.set_suppress(false);
        assert!(!p.suppressed());
    }

    #[test]
    fn injected_motion_is_not_physical() {
        let p = FakePlatform::new();
        let (tx, mut rx) = unbounded_channel();
        p.start_capture(tx).unwrap();
        p.push_injected_motion_to((10.0, 20.0));
        assert_eq!(p.cursor_pos(), (10.0, 20.0));
        let e = rx.try_recv().unwrap();
        assert!(!e.physical);
        assert!(matches!(e.event, InputEvent::Motion { .. }));
    }

    #[test]
    fn inject_records_and_tracks_held_keys() {
        let p = FakePlatform::new();
        p.inject(&InputEvent::Key {
            usage: keys::LEFT_SHIFT,
            down: true,
        })
        .unwrap();
        p.inject(&InputEvent::Key {
            usage: keys::A,
            down: true,
        })
        .unwrap();
        p.inject(&InputEvent::Key {
            usage: keys::A,
            down: true,
        })
        .unwrap(); // repeat
        let mut h = p.held_now();
        h.sort();
        assert_eq!(h, vec![keys::A, keys::LEFT_SHIFT]);
        p.inject(&InputEvent::Key {
            usage: keys::A,
            down: false,
        })
        .unwrap();
        assert_eq!(p.held_now(), vec![keys::LEFT_SHIFT]);
        assert!(p.injected_contains(&InputEvent::Key {
            usage: keys::A,
            down: false
        }));
        assert!(!p.injected_contains(&InputEvent::Button {
            button: MouseButton::Left,
            down: true
        }));
    }

    #[test]
    fn warp_and_permissions_and_displays_setters() {
        let p = FakePlatform::new();
        p.warp((7.0, 8.0));
        assert_eq!(p.last_warp(), Some((7.0, 8.0)));
        assert_eq!(p.cursor_pos(), (7.0, 8.0));
        p.set_permissions(vec![("accessibility", false)]);
        assert_eq!(p.permissions(), vec![("accessibility", false)]);
        p.set_displays(vec![]);
        assert!(p.displays().is_empty());
    }

    #[test]
    fn clone_shares_state() {
        let p = FakePlatform::new();
        let q = p.clone();
        q.set_suppress(true);
        assert!(p.suppressed());
    }
}
