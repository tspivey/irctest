//! Speech through prismer (screen reader if one is running, otherwise SAPI etc.).
//!
//! Lives on the GUI thread. Whether a given message is spoken is decided by [`should_speak`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use prismer::{Backend, Prism};

use crate::route::irc_lower;

pub struct Speech {
    /// The prism context lives for the whole program (backends borrow it).
    prism: Option<&'static Prism>,
    backend: Option<Backend<'static>>,
    /// Set by prism's availability poll thread when a backend appears or disappears
    /// (e.g. NVDA restarting); the best backend is picked again before the next speech.
    changed: Arc<AtomicBool>,
    /// Shift+F5: when off, no messages are spoken at all.
    pub enabled: bool,
    /// Why speech isn't available, if it isn't.
    pub error: Option<String>,
}

impl Speech {
    pub fn new() -> Self {
        let changed = Arc::new(AtomicBool::new(false));
        let flag = changed.clone();
        // With a callback set, prism polls backend availability every second on its own thread.
        let prism = Prism::builder()
            .on_availability(move |_, _, _| flag.store(true, Ordering::Relaxed))
            .build();
        let mut speech = Self { prism: None, backend: None, changed, enabled: true, error: None };
        match prism {
            Ok(prism) => {
                speech.prism = Some(&*Box::leak(Box::new(prism)));
                speech.pick_best();
            }
            Err(e) => speech.error = Some(format!("{e}")),
        }
        speech
    }

    /// Name of the backend in use, e.g. "NVDA" or "SAPI".
    pub fn backend_name(&self) -> Option<String> {
        self.backend.as_ref().map(Backend::name)
    }

    /// Switches to the highest-priority backend that's available now.
    fn pick_best(&mut self) {
        let Some(prism) = self.prism else { return };
        match prism.create_best() {
            Ok(backend) => {
                self.backend = Some(backend);
                self.error = None;
            }
            Err(e) => {
                self.backend = None;
                self.error = Some(format!("{e}"));
            }
        }
    }

    /// Speaks regardless of `enabled` (for feedback on the user's own actions).
    /// `interrupt` cuts off anything already being said.
    pub fn say(&mut self, text: &str, interrupt: bool) {
        // For testing: print instead of speaking.
        if std::env::var_os("IRCTEST_SPEECH_LOG").is_some() {
            println!("SPEAK{}: {text}", if interrupt { " (interrupt)" } else { "" });
            return;
        }
        if self.changed.swap(false, Ordering::Relaxed) {
            self.pick_best();
        }
        if self.backend.as_ref().is_some_and(|b| b.speak(text, interrupt).is_ok()) {
            return;
        }
        // The backend went away before the poll noticed (e.g. NVDA was just closed): pick the
        // best one again and retry once.
        self.pick_best();
        if let Some(backend) = &self.backend {
            let _ = backend.speak(text, interrupt);
        }
    }

    /// F4: cuts off whatever is being said. Some backends (e.g. Speech Dispatcher) don't stop
    /// when Ctrl is pressed the way a screen reader does.
    pub fn stop(&mut self) {
        if std::env::var_os("IRCTEST_SPEECH_LOG").is_some() {
            println!("SPEAK STOP");
            return;
        }
        if let Some(backend) = &self.backend {
            // Errors just mean nothing was being said or the backend can't stop.
            let _ = backend.stop();
        }
    }

    /// Speaks a message if `enabled`, queued behind anything already being said.
    pub fn message(&mut self, text: &str) {
        if self.enabled {
            self.say(text, false);
        }
    }
}

impl Default for Speech {
    fn default() -> Self {
        Self::new()
    }
}

/// Global speech off silences everything; otherwise speak if the tab's speech is on or the
/// message mentions us.
pub fn should_speak(global: bool, tab: bool, mentions_me: bool) -> bool {
    global && (tab || mentions_me)
}

/// Whether `nick` appears in `text` as a whole word, ignoring case (RFC 1459 casemapping).
/// "tyler: hi" and "hi Tyler!" match; "tylers" and "tyler_" don't.
pub fn mentions(text: &str, nick: &str) -> bool {
    if nick.is_empty() {
        return false;
    }
    // irc_lower maps ASCII to ASCII and leaves everything else alone, so byte offsets line up.
    let (text, nick) = (irc_lower(text), irc_lower(nick));
    text.match_indices(&nick).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + nick.len()..].chars().next();
        !before.is_some_and(is_nick_char) && !after.is_some_and(is_nick_char)
    })
}

/// Characters that can be part of a nick (after casefolding).
fn is_nick_char(c: char) -> bool {
    c.is_alphanumeric() || "-_[]\\`^{}|".contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speak_rules() {
        assert!(should_speak(true, true, false));
        assert!(!should_speak(true, false, false));
        assert!(should_speak(true, false, true), "mention beats tab speech off");
        assert!(!should_speak(false, true, true), "global off beats everything");
    }

    #[test]
    fn mention_matching() {
        assert!(mentions("tyler: hi", "tyler"));
        assert!(mentions("hi Tyler!", "tyler"));
        assert!(mentions("hey TYLER", "tyler"));
        assert!(mentions("(tyler)", "tyler"));
        assert!(mentions("ask tyler, or bob", "tyler"));
        assert!(!mentions("tylers are great", "tyler"));
        assert!(!mentions("tyler_ is away", "tyler"));
        assert!(!mentions("mytyler", "tyler"));
        assert!(mentions("ping tyler_", "tyler_"));
        assert!(mentions("Hi [Bob]", "{bob}"));
        assert!(mentions("café tyler", "tyler"));
        assert!(!mentions("anything", ""));
        assert!(mentions("tyler tylers", "tyler"));
    }
}
