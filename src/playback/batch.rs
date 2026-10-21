#[cfg(test)]
mod tests {
    use super::*;
    fn ingest(t: &mut BatchTracker, line: &str) -> Result<BatchEvent, String> {
        t.ingest_line(line)
    }
    #[test]
    fn nested_history_keeps_origin_while_live_is_live() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +h chathistory #test").unwrap();
        ingest(&mut t, "@batch=h BATCH +n example/nested").unwrap();
        assert!(
            matches!(ingest(&mut t, "@batch=n :bob!u@h PRIVMSG #test :old").unwrap(), BatchEvent::Message { history: true, batch: Some(b), target: Some(t), .. } if b == "h" && t == "#test")
        );
        assert!(matches!(
            ingest(&mut t, ":bob!u@h PRIVMSG #test :new").unwrap(),
            BatchEvent::Message { history: false, .. }
        ));
        assert!(ingest(&mut t, "BATCH -h").is_err());
        ingest(&mut t, "@batch=h BATCH -n").unwrap();
        ingest(&mut t, "BATCH -h").unwrap();
        assert!(ingest(&mut t, "@batch=h :b!u@h PRIVMSG #test :late").is_err());
    }
    #[test]
    fn rejects_ambiguous_and_excessive_batches() {
        let mut t = BatchTracker::default();
        assert!(ingest(&mut t, "@batch=missing BATCH +a chathistory #t").is_err());
        ingest(&mut t, "BATCH +a znc.in/playback").unwrap();
        assert!(ingest(&mut t, "BATCH +a chathistory #t").is_err());
        for i in 1..128 {
            ingest(&mut t, &format!("BATCH +b{i} unknown")).unwrap();
        }
        assert!(ingest(&mut t, "BATCH +overflow unknown").is_err());
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a0 unknown").unwrap();
        for i in 1..16 {
            ingest(&mut t, &format!("@batch=a{} BATCH +a{i} unknown", i - 1)).unwrap();
        }
        assert!(ingest(&mut t, "@batch=a15 BATCH +a16 unknown").is_err());
    }
    #[test]
    fn invalid_identifiers_and_missing_history_target_are_rejected() {
        for line in [
            "BATCH +bad_id unknown",
            "BATCH +🔥 unknown",
            "BATCH +missing chathistory",
        ] {
            assert!(
                ingest(&mut BatchTracker::default(), line).is_err(),
                "{line}"
            );
        }
        assert!(ingest(&mut BatchTracker::default(), "BATCH 🔥").is_err());
    }
    #[test]
    fn closing_with_an_unrelated_parent_does_not_remove_the_batch() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a unknown").unwrap();
        ingest(&mut t, "BATCH +b unknown").unwrap();
        assert!(ingest(&mut t, "@batch=b BATCH -a").is_err());
        ingest(&mut t, "BATCH -a").unwrap();
    }
    #[test]
    fn nested_close_requires_the_original_parent_tag() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a unknown").unwrap();
        ingest(&mut t, "@batch=a BATCH +b unknown").unwrap();
        assert!(ingest(&mut t, "BATCH -b").is_err());
        ingest(&mut t, "@batch=a BATCH -b").unwrap();
    }
    #[test]
    fn chathistory_requires_a_target() {
        assert!(ingest(&mut BatchTracker::default(), "BATCH +a chathistory").is_err());
    }
    #[test]
    fn malformed_or_ambiguous_parent_tags_never_become_live() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a chathistory #test").unwrap();
        for line in [
            "@batch PRIVMSG #test :old",
            "@batch= PRIVMSG #test :old",
            "@batch=a;batch=missing PRIVMSG #test :old",
        ] {
            assert!(ingest(&mut t, line).is_err(), "{line}");
        }
    }
    #[test]
    fn raw_type_survives_tags_prefix_spaces_and_trailing_parameter() {
        let mut t = BatchTracker::default();
        let event = ingest(&mut t, "@label=value :server BATCH +a :chathistory");
        assert!(event.is_err()); // a real history type still needs a target
        ingest(&mut t, "@label=value :server BATCH +a  chathistory #test").unwrap();
        assert!(matches!(
            ingest(&mut t, "@batch=a PRIVMSG #test :old").unwrap(),
            BatchEvent::Message { history: true, .. }
        ));
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a :chathistory unknown").unwrap();
        assert!(matches!(
            ingest(&mut t, "@batch=a PRIVMSG #test :live").unwrap(),
            BatchEvent::Message { history: false, .. }
        ));
    }
    #[test]
    fn batch_type_is_opaque_and_case_sensitive() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a CHATHISTORY #t").unwrap();
        assert!(matches!(
            ingest(&mut t, "@batch=a PRIVMSG #t :live").unwrap(),
            BatchEvent::Message { history: false, .. }
        ));
    }
    #[test]
    fn boundaries_carry_parent_and_historical_ancestry() {
        let mut t = BatchTracker::default();
        assert!(
            matches!(ingest(&mut t, "BATCH +h chathistory #test").unwrap(), BatchEvent::Boundary(BatchBoundary::Start(meta)) if meta.history && meta.parent.is_none())
        );
        assert!(
            matches!(ingest(&mut t, "@batch=h BATCH +n soju.im/bouncer-networks").unwrap(), BatchEvent::Boundary(BatchBoundary::Start(meta)) if meta.history && meta.parent.as_deref()==Some("h"))
        );
        assert!(
            matches!(ingest(&mut t, "@batch=h BATCH -n").unwrap(), BatchEvent::Boundary(BatchBoundary::End(meta)) if meta.history && meta.parent.as_deref()==Some("h"))
        );
    }
    #[test]
    fn close_exhaustion_tag_reaches_history_scheduler() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a chathistory #test").unwrap();
        assert!(
            matches!(ingest(&mut t, "@draft/chathistory-end BATCH -a").unwrap(), BatchEvent::Boundary(BatchBoundary::End(meta)) if meta.exhausted)
        );
    }
    #[test]
    fn nested_history_uses_outer_history_owner() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +a chathistory #outer").unwrap();
        ingest(&mut t, "@batch=a BATCH +b chathistory #inner").unwrap();
        assert!(
            matches!(ingest(&mut t, "@batch=b PRIVMSG #outer :old").unwrap(), BatchEvent::Message { batch: Some(id), target: Some(target), .. } if id=="a" && target=="#outer")
        );
    }
    #[test]
    fn identifiers_are_case_sensitive_unknown_batches_live_and_znc_quiet() {
        let mut t = BatchTracker::default();
        ingest(&mut t, "BATCH +ID example/unknown").unwrap();
        ingest(&mut t, "BATCH +id znc.in/playback").unwrap();
        assert!(matches!(
            ingest(&mut t, "@batch=ID PRIVMSG #t :live").unwrap(),
            BatchEvent::Message { history: false, .. }
        ));
        assert!(matches!(
            ingest(&mut t, "@batch=id PRIVMSG #t :old").unwrap(),
            BatchEvent::Message {
                history: true,
                target: None,
                ..
            }
        ));
        ingest(&mut t, "BATCH -id").unwrap();
    }
}

use irc_proto::{Command, Message};
use std::collections::HashMap;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMeta {
    pub id: String,
    pub kind: String,
    pub target: Option<String>,
    pub exhausted: bool,
    pub parent: Option<String>,
    /// True for a historical batch and every batch nested within one.
    pub history: bool,
}
#[derive(Debug, Clone)]
pub enum BatchBoundary {
    Start(BatchMeta),
    End(BatchMeta),
}
#[derive(Debug, Clone)]
pub enum BatchEvent {
    Boundary(BatchBoundary),
    Message {
        message: Message,
        history: bool,
        batch: Option<String>,
        target: Option<String>,
    },
}
#[derive(Debug)]
struct Entry {
    meta: BatchMeta,
    parent: Option<String>,
}
#[derive(Debug, Default)]
pub struct BatchTracker {
    entries: HashMap<String, Entry>,
}
pub fn tag<'a>(message: &'a Message, name: &str) -> Option<&'a str> {
    message
        .tags
        .as_ref()?
        .iter()
        .find(|t| t.0 == name)?
        .1
        .as_deref()
}
fn raw_batch_kind(line: &str) -> Option<String> {
    fn word(line: &str) -> (&str, &str) {
        let line = line.trim_start_matches(' ');
        let (first, rest) = line.split_once(' ').unwrap_or((line, ""));
        (first, rest.trim_start_matches(' '))
    }
    let mut rest = line.trim_end_matches(['\r', '\n']).trim_start_matches(' ');
    if rest.starts_with('@') {
        rest = word(rest).1;
    }
    if rest.starts_with(':') {
        rest = word(rest).1;
    }
    rest = word(rest).1; // command
    rest = word(rest).1; // batch reference
    if rest.is_empty() {
        return None;
    }
    Some(
        rest.strip_prefix(':')
            .unwrap_or_else(|| word(rest).0)
            .to_owned(),
    )
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn is_history(kind: &str) -> bool {
    matches!(
        kind,
        "chathistory" | "znc.in/playback" | "draft/chathistory-targets"
    )
}
fn has_end_tag(message: &Message) -> bool {
    message
        .tags
        .as_ref()
        .is_some_and(|tags| tags.iter().any(|t| t.0 == "draft/chathistory-end"))
}
impl BatchTracker {
    /// Preserve the opaque batch type before irc-proto uppercases it.
    pub fn ingest_line(&mut self, line: &str) -> Result<BatchEvent, String> {
        let message = line
            .parse::<Message>()
            .map_err(|_| "Invalid IRC message.")?;
        let kind = if matches!(message.command, Command::BATCH(..)) {
            raw_batch_kind(line)
        } else {
            None
        };
        self.ingest_with_kind(message, kind)
    }

    /// Compatibility path for already parsed messages. irc-proto has discarded
    /// custom type case, so raw network input should use ingest_line instead.
    pub fn ingest(&mut self, message: Message) -> Result<BatchEvent, String> {
        self.ingest_with_kind(message, None)
    }

    fn ingest_with_kind(
        &mut self,
        message: Message,
        raw_kind: Option<String>,
    ) -> Result<BatchEvent, String> {
        if let Some(tags) = &message.tags {
            let mut parents = tags.iter().filter(|t| t.0 == "batch");
            if let Some(first) = parents.next() {
                if first.1.as_deref().is_none_or(str::is_empty) || parents.next().is_some() {
                    return Err("Invalid batch parent tag.".into());
                }
            }
        }
        let parent = tag(&message, "batch").map(str::to_owned);
        let mut ancestors = Vec::new();
        let mut at = parent.as_deref();
        while let Some(id) = at {
            let e = self.entries.get(id).ok_or("Unknown batch reference.")?;
            ancestors.push(e);
            at = e.parent.as_deref();
            if ancestors.len() > 16 {
                return Err("Batch nesting limit exceeded.".into());
            }
        }
        if let Command::BATCH(reference, kind, params) = &message.command {
            if let Some(id) = reference.strip_prefix('+') {
                if !valid_id(id)
                    || self.entries.contains_key(id)
                    || self.entries.len() >= 128
                    || ancestors.len() >= 16
                {
                    return Err("Invalid or excessive batch start.".into());
                }
                let kind = raw_kind.unwrap_or_else(|| {
                    kind.as_ref()
                        .map(|k| k.to_str().to_ascii_lowercase())
                        .unwrap_or_default()
                });
                if kind.is_empty() {
                    return Err("Batch start has no type.".into());
                }
                let target = params.as_ref().and_then(|p| p.first().cloned());
                if kind == "chathistory" && target.as_ref().is_none_or(|t| t.is_empty()) {
                    return Err("Missing history batch target.".into());
                }
                let history = is_history(&kind) || ancestors.iter().any(|e| e.meta.history);
                let meta = BatchMeta {
                    id: id.into(),
                    kind,
                    target,
                    parent: parent.clone(),
                    history,
                    exhausted: has_end_tag(&message),
                };
                self.entries.insert(
                    id.into(),
                    Entry {
                        meta: meta.clone(),
                        parent,
                    },
                );
                return Ok(BatchEvent::Boundary(BatchBoundary::Start(meta)));
            }
            if let Some(id) = reference.strip_prefix('-') {
                if !valid_id(id) {
                    return Err("Invalid batch identifier.".into());
                }
                let current = self.entries.get(id).ok_or("Unknown batch end.")?;
                if parent != current.parent {
                    return Err("Batch closed under a different parent.".into());
                }
                if self
                    .entries
                    .values()
                    .any(|e| e.parent.as_deref() == Some(id))
                {
                    return Err("Batch closed before its children.".into());
                }
                let mut entry = self.entries.remove(id).ok_or("Unknown batch end.")?;
                entry.meta.exhausted |= has_end_tag(&message);
                return Ok(BatchEvent::Boundary(BatchBoundary::End(entry.meta)));
            }
            return Err("Invalid batch reference.".into());
        }
        let origin = ancestors.iter().rev().find(|e| is_history(&e.meta.kind));
        let history = origin.is_some();
        let batch = origin.map(|e| e.meta.id.clone()).or(parent);
        let target = origin.and_then(|e| e.meta.target.clone());
        Ok(BatchEvent::Message {
            message,
            history,
            batch,
            target,
        })
    }
}
