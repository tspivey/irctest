//! Up/Down recall of lines typed into an input box.

const MAX_ENTRIES: usize = 100;

#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    /// Index into `entries` while browsing; `None` when at the line being typed.
    pos: Option<usize>,
    /// What was in the box before Up was first pressed, restored by Down past the newest entry.
    draft: String,
}

impl History {
    /// Records a sent line and stops browsing. Blank lines and repeats of the newest entry
    /// aren't recorded.
    pub fn push(&mut self, line: &str) {
        self.pos = None;
        self.draft.clear();
        if line.trim().is_empty() || self.entries.last().is_some_and(|last| last == line) {
            return;
        }
        self.entries.push(line.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
    }

    /// Up: the previous (older) entry, or `None` if there's nothing older. `current` is what's
    /// in the box now, kept as the draft when browsing starts.
    pub fn up(&mut self, current: &str) -> Option<String> {
        let pos = match self.pos {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(p) => p - 1,
        };
        self.pos = Some(pos);
        Some(self.entries[pos].clone())
    }

    /// Down: the next (newer) entry, then the draft; `None` if not browsing.
    pub fn down(&mut self) -> Option<String> {
        let pos = self.pos?;
        if pos + 1 < self.entries.len() {
            self.pos = Some(pos + 1);
            Some(self.entries[pos + 1].clone())
        } else {
            self.pos = None;
            Some(std::mem::take(&mut self.draft))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history() {
        let mut h = History::default();
        assert_eq!(h.up("typing"), None);
        assert_eq!(h.down(), None);
    }

    #[test]
    fn browse_and_return_to_draft() {
        let mut h = History::default();
        h.push("one");
        h.push("two");
        h.push("three");
        assert_eq!(h.up("draft").as_deref(), Some("three"));
        assert_eq!(h.up("three").as_deref(), Some("two"));
        assert_eq!(h.up("two").as_deref(), Some("one"));
        assert_eq!(h.up("one"), None, "stops at the oldest");
        assert_eq!(h.down().as_deref(), Some("two"));
        assert_eq!(h.down().as_deref(), Some("three"));
        assert_eq!(h.down().as_deref(), Some("draft"));
        assert_eq!(h.down(), None);
    }

    #[test]
    fn push_skips_blanks_and_repeats_and_resets() {
        let mut h = History::default();
        h.push("a");
        h.push("a");
        h.push("   ");
        h.push("b");
        assert_eq!(h.up("").as_deref(), Some("b"));
        h.push("c");
        assert_eq!(h.up("").as_deref(), Some("c"), "push resets browsing");
        assert_eq!(h.up("").as_deref(), Some("b"));
        assert_eq!(h.up("").as_deref(), Some("a"));
        assert_eq!(h.up(""), None);
    }

    #[test]
    fn capped() {
        let mut h = History::default();
        for i in 0..150 {
            h.push(&i.to_string());
        }
        let mut oldest = String::new();
        while let Some(line) = h.up("") {
            oldest = line;
        }
        assert_eq!(oldest, "50");
    }
}
