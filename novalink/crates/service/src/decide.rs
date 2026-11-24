//! Pure decisions for the service: which helper process should be running.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum HelperAction {
    Start(u32),
    Stop,
    Restart(u32),
    Nothing,
}

/// `console` is the active console session, `running` the session the helper was
/// started for, `exited` whether that helper process has ended.
pub fn helper_action(console: Option<u32>, running: Option<u32>, exited: bool) -> HelperAction {
    match (console, running) {
        (None, Some(_)) => HelperAction::Stop,
        (None, None) => HelperAction::Nothing,
        (Some(c), None) => HelperAction::Start(c),
        (Some(c), Some(r)) if c != r || exited => HelperAction::Restart(c),
        (Some(_), Some(_)) => HelperAction::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_follows_console_session() {
        assert_eq!(helper_action(Some(1), None, false), HelperAction::Start(1));
        assert_eq!(helper_action(Some(1), Some(1), false), HelperAction::Nothing);
        assert_eq!(helper_action(Some(2), Some(1), false), HelperAction::Restart(2));
        assert_eq!(helper_action(Some(1), Some(1), true), HelperAction::Restart(1));
        assert_eq!(helper_action(None, Some(1), false), HelperAction::Stop);
        assert_eq!(helper_action(None, None, false), HelperAction::Nothing);
    }
}
