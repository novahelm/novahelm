//! Pairing prompts: a message box per request, each on its own thread.

use super::system;
use novalink_client::{Client, PendingPair};
use novalink_tray::model::{error_text, pair_prompt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The (peer, code) pairs still waiting for an answer. A box whose request has left this set
/// was withdrawn, and whatever it answers is ignored.
pub type Live = Arc<Mutex<HashMap<String, String>>>;

pub fn show(p: PendingPair, live: Live) {
    std::thread::spawn(move || {
        let (title, text) = pair_prompt(&p);
        let accept = system::ask_default_no(&title, &text);
        let still_live = live
            .lock()
            .map(|m| m.get(&p.peer) == Some(&p.code))
            .unwrap_or(false);
        if !still_live {
            return;
        }
        let result = Client::local().and_then(|c| c.answer(&p.peer, accept));
        if let Err(e) = result {
            system::info("NovaLink", &error_text(&e));
        }
    });
}
