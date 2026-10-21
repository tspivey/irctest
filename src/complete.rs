//! Tab completion of nicknames in the Message input.
//!
//! Text positions here are byte offsets into the input's value; the GUI converts the
//! control's cursor position (characters, or UTF-16 units on Windows) to and from them.

use crate::route::irc_lower;

/// A completion in progress, so another Tab can replace it with the next match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The input up to the word being completed.
    before: String,
    /// The input after the cursor when completion started.
    after: String,
    /// Matching nicks, best first.
    matches: Vec<String>,
    index: usize,
}

impl Completion {
    /// Starts completing the word before `cursor` in `text`. `nicks` are the candidates in
    /// cycling order. `None` if there's no word or nothing matches it.
    pub fn start(text: &str, cursor: usize, nicks: &[String]) -> Option<Self> {
        let cursor = floor_char_boundary(text, cursor);
        let start = text[..cursor].rfind(char::is_whitespace).map_or(0, |i| {
            i + text[i..].chars().next().map_or(1, char::len_utf8)
        });
        let word = irc_lower(&text[start..cursor]);
        if word.is_empty() {
            return None;
        }
        let matches: Vec<String> = nicks
            .iter()
            .filter(|nick| irc_lower(nick).starts_with(&word))
            .cloned()
            .collect();
        if matches.is_empty() {
            return None;
        }
        Some(Self {
            before: text[..start].to_string(),
            after: text[cursor..].to_string(),
            matches,
            index: 0,
        })
    }

    /// Moves to the next match, wrapping around.
    pub fn next(&mut self) {
        self.index = (self.index + 1) % self.matches.len();
    }

    /// The current match.
    pub fn nick(&self) -> &str {
        &self.matches[self.index]
    }

    /// The input with the current match in place, and the cursor (a byte offset) just after
    /// the match and its suffix: ": " at the start of the line, otherwise " ".
    pub fn text(&self) -> (String, usize) {
        let suffix = if self.before.is_empty() { ": " } else { " " };
        let head = format!("{}{}{suffix}", self.before, self.nick());
        let cursor = head.len();
        (head + &self.after, cursor)
    }
}

/// Orders channel members for completion: most recent speakers first, then everyone else
/// alphabetically. `spoke` gives when each `irc_lower` nick last spoke, if known.
pub fn order<T: Ord + Copy>(
    nicks: impl IntoIterator<Item = String>,
    spoke: impl Fn(&str) -> Option<T>,
) -> Vec<String> {
    let mut nicks: Vec<(Option<T>, String, String)> = nicks
        .into_iter()
        .map(|nick| {
            let lower = irc_lower(&nick);
            (spoke(&lower), lower, nick)
        })
        .collect();
    nicks.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    nicks.into_iter().map(|(_, _, nick)| nick).collect()
}

fn floor_char_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nicks(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn completes_at_line_start_with_colon() {
        let c = Completion::start("bo", 2, &nicks(&["alice", "Bob"])).unwrap();
        assert_eq!(c.text(), ("Bob: ".to_string(), 5));
    }

    #[test]
    fn completes_mid_line_with_space_and_keeps_text_after_cursor() {
        let c = Completion::start("hi al there", 5, &nicks(&["Alice"])).unwrap();
        assert_eq!(c.text(), ("hi Alice  there".to_string(), 9));
    }

    #[test]
    fn cycles_and_wraps() {
        let mut c = Completion::start("b", 1, &nicks(&["bob", "alice", "barry"])).unwrap();
        assert_eq!(c.nick(), "bob");
        c.next();
        assert_eq!(c.nick(), "barry");
        c.next();
        assert_eq!(c.nick(), "bob");
    }

    #[test]
    fn nothing_to_complete() {
        assert!(Completion::start("", 0, &nicks(&["bob"])).is_none());
        assert!(Completion::start("hi ", 3, &nicks(&["bob"])).is_none());
        assert!(Completion::start("zed", 3, &nicks(&["bob"])).is_none());
    }

    #[test]
    fn rfc1459_case_folding() {
        let c = Completion::start("[a", 2, &nicks(&["{Away}"])).unwrap();
        assert_eq!(c.nick(), "{Away}");
    }

    #[test]
    fn non_ascii_word() {
        let c = Completion::start("salut é", "salut é".len(), &nicks(&["éric"])).unwrap();
        assert_eq!(c.text().0, "salut éric ");
    }

    #[test]
    fn recent_speakers_first_then_alphabetical() {
        let spoke = |n: &str| match n {
            "carol" => Some(1),
            "bob" => Some(2),
            _ => None,
        };
        let ordered = order(nicks(&["dave", "Carol", "alice", "bob"]), spoke);
        assert_eq!(ordered, nicks(&["bob", "Carol", "alice", "dave"]));
    }
}
