//! How the browser panel gets in without ever holding the long-lived token, which would end
//! up in browser history (and history sync) if it rode in the URL. A client that has proved
//! the agent asks for a code; the panel URL carries only that code, which works once and only
//! for a minute; the page trades it for a session token it keeps in sessionStorage.

use novagear_core::proof::{new_nonce, same};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const CODE_TTL: Duration = Duration::from_secs(60);
/// Old sessions fall off the end; a panel past this many opens just needs reopening.
const MAX_SESSIONS: usize = 16;

#[derive(Default)]
pub struct PanelAuth {
    codes: Vec<(String, Instant)>,
    sessions: VecDeque<String>,
}

impl PanelAuth {
    pub fn issue_code(&mut self, now: Instant) -> anyhow::Result<String> {
        self.codes
            .retain(|(_, at)| now.duration_since(*at) < CODE_TTL);
        let code = new_nonce()?;
        self.codes.push((code.clone(), now));
        Ok(code)
    }

    /// A fresh session token for a live, unused code; the code is spent either way.
    pub fn redeem(&mut self, code: &str, now: Instant) -> Option<String> {
        let i = self.codes.iter().position(|(c, _)| same(c, code))?;
        let (_, at) = self.codes.swap_remove(i);
        if now.duration_since(at) >= CODE_TTL {
            return None;
        }
        let session = format!("{}{}", new_nonce().ok()?, new_nonce().ok()?);
        self.sessions.push_back(session.clone());
        while self.sessions.len() > MAX_SESSIONS {
            self.sessions.pop_front();
        }
        Some(session)
    }

    pub fn is_session(&self, token: &str) -> bool {
        // Check every entry so timing doesn't say which one matched.
        self.sessions
            .iter()
            .fold(false, |hit, s| same(s, token) | hit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_works_once_and_only_while_fresh() {
        let mut a = PanelAuth::default();
        let t0 = Instant::now();
        let code = a.issue_code(t0).unwrap();
        let session = a
            .redeem(&code, t0 + Duration::from_secs(5))
            .expect("fresh code redeems");
        assert!(a.is_session(&session));
        assert!(!a.is_session("nope"));
        assert_eq!(
            a.redeem(&code, t0 + Duration::from_secs(6)),
            None,
            "single use"
        );

        let late = a.issue_code(t0).unwrap();
        assert_eq!(a.redeem(&late, t0 + CODE_TTL), None, "expired");
        assert_eq!(a.redeem("not-a-code", t0), None);
    }

    #[test]
    fn keeps_only_recent_sessions() {
        let mut a = PanelAuth::default();
        let now = Instant::now();
        let first = {
            let c = a.issue_code(now).unwrap();
            a.redeem(&c, now).unwrap()
        };
        for _ in 0..MAX_SESSIONS {
            let c = a.issue_code(now).unwrap();
            a.redeem(&c, now).unwrap();
        }
        assert!(!a.is_session(&first), "the oldest session was dropped");
    }
}
