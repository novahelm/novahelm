use novalink_proto::{Control, DeviceId, EntryPoint};

/// Largest clock jump accepted from a peer in a single claim.
const MAX_CLOCK_JUMP: u64 = 1_000_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Broadcast(Control),
    Send(DeviceId, Control),
    /// Swallow local physical input and freeze or hide the cursor.
    SetSuppress(bool),
    WarpTo(EntryPoint),
    /// Release everything this machine injected for a remote controller.
    ReleaseInjected,
    /// After regaining focus, re-sync modifier state (emitted by the platform layer).
    ResetLocalSync,
}

#[derive(Debug)]
pub struct ControlState {
    me: DeviceId,
    controller: DeviceId,
    focus: DeviceId,
    clock: u64,
}

impl ControlState {
    pub fn new(me: DeviceId) -> Self {
        Self {
            me,
            controller: me,
            focus: me,
            clock: 0,
        }
    }
    pub fn controller(&self) -> DeviceId {
        self.controller
    }

    pub fn focus(&self) -> DeviceId {
        self.focus
    }

    /// True while this machine controls but another machine has the focus.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    pub fn suppressing(&self) -> bool {
        self.controller == self.me && self.focus != self.me
    }

    pub fn on_local_physical(&mut self) -> Vec<Action> {
        if self.controller == self.me {
            return Vec::new();
        }
        self.clock = self.clock.saturating_add(1);
        self.controller = self.me;
        self.focus = self.me;
        vec![
            Action::ReleaseInjected,
            Action::Broadcast(Control::Claim { clock: self.clock }),
            Action::SetSuppress(false),
        ]
    }

    pub fn on_claim(&mut self, from: DeviceId, clock: u64) -> Vec<Action> {
        // A wildly-ahead clock would lock this machine out of future claims.
        if clock > self.clock.saturating_add(MAX_CLOCK_JUMP) {
            return Vec::new();
        }
        let accept = clock > self.clock || (clock == self.clock && from > self.me);
        self.clock = self.clock.max(clock);
        if !accept {
            // Tell the rejected peer who controls, so both sides converge.
            if self.controller == self.me {
                return vec![Action::Send(from, Control::Claim { clock: self.clock })];
            }
            return Vec::new();
        }
        let mut actions = Vec::new();
        if self.suppressing() {
            actions.push(Action::Send(self.focus, Control::Leave));
            actions.push(Action::SetSuppress(false));
        } else if self.controller != self.me {
            // Release must not depend on the old controller's Leave arriving.
            actions.push(Action::ReleaseInjected);
        }
        self.controller = from;
        self.focus = self.me;
        actions
    }

    pub fn cross_to(&mut self, to: DeviceId, entry: EntryPoint) -> Vec<Action> {
        if self.controller != self.me {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if self.focus != self.me {
            actions.push(Action::Send(self.focus, Control::Leave));
        }
        if to == self.me {
            if self.focus != self.me {
                actions.push(Action::SetSuppress(false));
            }
            actions.push(Action::WarpTo(entry));
        } else {
            if self.focus == self.me {
                actions.push(Action::SetSuppress(true));
                // Crossing out from local is a claim: any other controller must stand down.
                self.clock = self.clock.saturating_add(1);
                actions.push(Action::Broadcast(Control::Claim { clock: self.clock }));
            }
            actions.push(Action::Send(to, Control::Enter { entry }));
        }
        self.focus = to;
        actions
    }

    pub fn on_enter(&mut self, from: DeviceId, entry: EntryPoint) -> Vec<Action> {
        self.controller = from;
        self.focus = self.me;
        vec![Action::WarpTo(entry)]
    }

    pub fn on_leave(&mut self, _from: DeviceId) -> Vec<Action> {
        vec![Action::ReleaseInjected]
    }

    pub fn on_peer_lost(&mut self, peer: DeviceId) -> Vec<Action> {
        let mut actions = Vec::new();
        if peer == self.focus && self.controller == self.me && peer != self.me {
            self.focus = self.me;
            actions.push(Action::SetSuppress(false));
        }
        if peer == self.controller && peer != self.me {
            self.controller = self.me;
            actions.push(Action::ReleaseInjected);
        }
        actions
    }

    #[cfg(test)]
    fn on_local_physical_force_clock(&mut self, clock: u64) {
        self.clock = clock;
        self.controller = self.me;
        self.focus = self.me;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novalink_proto::Edge;

    const A: DeviceId = DeviceId([1; 32]);
    const B: DeviceId = DeviceId([2; 32]);
    const C: DeviceId = DeviceId([3; 32]);
    fn ep() -> EntryPoint {
        EntryPoint {
            edge: Edge::Left,
            frac: 0.5,
        }
    }

    #[test]
    fn cross_out_suppresses_and_enters() {
        let mut s = ControlState::new(A);
        assert_eq!(
            s.cross_to(B, ep()),
            vec![
                Action::SetSuppress(true),
                Action::Broadcast(Control::Claim { clock: 1 }),
                Action::Send(B, Control::Enter { entry: ep() })
            ]
        );
        assert_eq!(s.focus(), B);
        assert!(s.suppressing());
    }
    #[test]
    fn hop_a_b_c_leaves_b() {
        let mut s = ControlState::new(A);
        s.cross_to(B, ep());
        assert_eq!(
            s.cross_to(C, ep()),
            vec![
                Action::Send(B, Control::Leave),
                Action::Send(C, Control::Enter { entry: ep() })
            ]
        );
    }
    #[test]
    fn come_home_unsuppresses() {
        let mut s = ControlState::new(A);
        s.cross_to(B, ep());
        assert_eq!(
            s.cross_to(A, ep()),
            vec![
                Action::Send(B, Control::Leave),
                Action::SetSuppress(false),
                Action::WarpTo(ep())
            ]
        );
        assert!(!s.suppressing());
    }
    #[test]
    fn touching_remote_hardware_takes_control() {
        let mut b = ControlState::new(B);
        b.on_claim(A, 1);
        let acts = b.on_local_physical();
        assert_eq!(
            acts,
            vec![
                Action::ReleaseInjected,
                Action::Broadcast(Control::Claim { clock: 2 }),
                Action::SetSuppress(false)
            ]
        );
        assert_eq!(b.controller(), B);
        assert!(b.on_local_physical().is_empty());
    }
    #[test]
    fn controller_receiving_claim_gives_up_focus_and_unsuppresses() {
        let mut a = ControlState::new(A);
        a.cross_to(B, ep());
        assert_eq!(
            a.on_claim(B, 1),
            vec![Action::Send(B, Control::Leave), Action::SetSuppress(false)]
        );
        assert_eq!((a.controller(), a.focus()), (B, A));
    }
    #[test]
    fn equal_clock_tiebreak_higher_id_wins() {
        let mut a = ControlState::new(A);
        a.on_local_physical_force_clock(1);
        // Accepted, but A's focus was local so there is nothing to hand back.
        assert!(a.on_claim(B, 1).is_empty());
        assert_eq!(a.controller(), B);
        let mut c = ControlState::new(C);
        c.on_local_physical_force_clock(1);
        assert_eq!(
            c.on_claim(B, 1),
            vec![Action::Send(B, Control::Claim { clock: 1 })]
        );
        assert_eq!(c.controller(), C);
    }
    #[test]
    fn focus_peer_lost_returns_cursor() {
        let mut a = ControlState::new(A);
        a.cross_to(B, ep());
        assert_eq!(a.on_peer_lost(B), vec![Action::SetSuppress(false)]);
        assert_eq!(a.focus(), A);
    }
    #[test]
    fn controller_lost_releases_injected() {
        let mut b = ControlState::new(B);
        b.on_enter(A, ep());
        assert_eq!(b.on_peer_lost(A), vec![Action::ReleaseInjected]);
        assert_eq!(b.controller(), B);
    }
    #[test]
    fn enter_warps_and_leave_releases() {
        let mut b = ControlState::new(B);
        assert_eq!(b.on_enter(A, ep()), vec![Action::WarpTo(ep())]);
        assert_eq!(b.on_leave(A), vec![Action::ReleaseInjected]);
    }

    #[test]
    fn enter_alone_records_driver_so_touch_claims() {
        let mut b = ControlState::new(B);
        b.on_enter(A, ep());
        assert_eq!(b.controller(), A);
        assert_eq!(
            b.on_local_physical(),
            vec![
                Action::ReleaseInjected,
                Action::Broadcast(Control::Claim { clock: 1 }),
                Action::SetSuppress(false)
            ]
        );
    }
    #[test]
    fn claim_from_third_peer_releases_injected() {
        let mut b = ControlState::new(B);
        b.on_enter(A, ep());
        assert_eq!(b.on_claim(C, 1), vec![Action::ReleaseInjected]);
        assert_eq!(b.controller(), C);
    }
    #[test]
    fn rejected_claim_replies_with_own_clock_and_converges() {
        let mut a = ControlState::new(A);
        a.on_local_physical_force_clock(5);
        assert_eq!(
            a.on_claim(C, 4),
            vec![Action::Send(C, Control::Claim { clock: 5 })]
        );
        let mut c = ControlState::new(C);
        c.on_local_physical_force_clock(4);
        c.on_claim(A, 5);
        assert_eq!(c.controller(), A);
        assert_eq!(
            c.on_local_physical(),
            vec![
                Action::ReleaseInjected,
                Action::Broadcast(Control::Claim { clock: 6 }),
                Action::SetSuppress(false)
            ]
        );
    }
    #[test]
    fn absurd_claim_clock_is_ignored() {
        let mut a = ControlState::new(A);
        assert!(a.on_claim(B, u64::MAX).is_empty());
        assert_eq!(a.controller(), A);
        a.on_enter(B, ep());
        assert_eq!(
            a.on_local_physical(),
            vec![
                Action::ReleaseInjected,
                Action::Broadcast(Control::Claim { clock: 1 }),
                Action::SetSuppress(false)
            ]
        );
    }

    #[test]
    fn crossing_out_from_local_claims_so_old_controller_stands_down() {
        // C was driven by A; C's own mouse now crosses into B.
        let mut c = ControlState::new(C);
        c.on_enter(A, ep());
        c.on_local_physical(); // takes control back (clock 1)
        let acts = c.cross_to(B, ep());
        assert_eq!(
            acts,
            vec![
                Action::SetSuppress(true),
                Action::Broadcast(Control::Claim { clock: 2 }),
                Action::Send(B, Control::Enter { entry: ep() })
            ]
        );
        assert_eq!(c.clock(), 2);
    }
}
