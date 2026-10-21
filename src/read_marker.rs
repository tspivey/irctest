use crate::playback::tag;
use crate::route::irc_lower;
use crate::sync_state::{ConversationState, timestamp};
use irc_proto::{Command, Message};
use std::collections::HashMap;
fn valid_target(target: &str) -> Result<(), String> {
    if target.is_empty()
        || target.starts_with(':')
        || target == "*"
        || target
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ',')
    {
        return Err("Invalid read marker target.".into());
    }
    Ok(())
}
#[derive(Debug, Clone)]
pub struct Update {
    pub target: String,
    pub time: Option<String>,
}
pub fn parse(message: &Message) -> Result<Option<Update>, String> {
    let Command::Raw(command, args) = &message.command else {
        return Ok(None);
    };
    if !command.eq_ignore_ascii_case("MARKREAD") {
        return Ok(None);
    }
    if args.len() != 2 {
        return Err("Invalid read marker.".into());
    }
    valid_target(&args[0])?;
    let time = if args[1] == "*" {
        None
    } else {
        let value = args[1]
            .strip_prefix("timestamp=")
            .ok_or("Invalid read marker selector.")?;
        timestamp(value)?;
        Some(value.into())
    };
    Ok(Some(Update {
        target: args[0].clone(),
        time,
    }))
}
#[derive(Debug, Default)]
pub struct Tracker {
    read: HashMap<String, ConversationState>,
    latest: HashMap<String, String>,
}
impl Tracker {
    pub fn restore(&mut self, target: &str, read: Option<&str>) -> Result<(), String> {
        valid_target(target)?;
        if let Some(t) = read {
            self.remote(Update {
                target: target.into(),
                time: Some(t.into()),
            })?;
        }
        Ok(())
    }
    pub fn remote(&mut self, update: Update) -> Result<bool, String> {
        valid_target(&update.target)?;
        let Some(time) = update.time else {
            return Ok(false);
        };
        self.read
            .entry(irc_lower(&update.target))
            .or_default()
            .mark_read(&time)
    }
    pub fn observe(&mut self, target: &str, message: &Message) {
        if valid_target(target).is_err() {
            return;
        }
        if !matches!(message.command, Command::PRIVMSG(..) | Command::NOTICE(..)) {
            return;
        }
        let Some(time) = tag(message, "time") else {
            return;
        };
        let Ok(new) = timestamp(time) else {
            return;
        };
        let slot = self.latest.entry(irc_lower(target)).or_default();
        if timestamp(slot).ok().is_none_or(|old| new >= old) {
            *slot = time.into();
        }
    }
    pub fn mark(&mut self, target: &str) -> Result<Message, String> {
        valid_target(target)?;
        let time = self
            .latest
            .get(&irc_lower(target))
            .ok_or("No displayed chat message has a server timestamp.")?;
        Ok(Command::Raw(
            "MARKREAD".into(),
            vec![target.into(), format!("timestamp={time}")],
        )
        .into())
    }
    pub fn latest(&self, target: &str) -> Option<&str> {
        self.latest.get(&irc_lower(target)).map(String::as_str)
    }
    pub fn forget_latest(&mut self, target: &str) {
        self.latest.remove(&irc_lower(target));
    }
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        valid_target(old)?;
        valid_target(new)?;
        let old = irc_lower(old);
        let new = irc_lower(new);
        if old == new {
            return Ok(());
        }
        if let Some(state) = self.read.remove(&old) {
            if let Some(time) = state.read {
                self.read.entry(new.clone()).or_default().mark_read(&time)?;
            }
        }
        if let Some(time) = self.latest.remove(&old) {
            let slot = self.latest.entry(new).or_default();
            if timestamp(slot)
                .ok()
                .is_none_or(|previous| timestamp(&time).is_ok_and(|time| time > previous))
            {
                *slot = time;
            }
        }
        Ok(())
    }
    pub fn read(&self, target: &str) -> Option<&str> {
        self.read.get(&irc_lower(target))?.read.as_deref()
    }
    pub fn unread(&self, target: &str) -> bool {
        let Some(latest) = self
            .latest
            .get(&irc_lower(target))
            .and_then(|t| timestamp(t).ok())
        else {
            return false;
        };
        self.read(target)
            .and_then(|t| timestamp(t).ok())
            .is_none_or(|read| latest > read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_rename_preserves_and_merges_monotonic_positions() {
        let mut tracker = Tracker::default();
        tracker
            .restore("Old", Some("2026-10-20T09:00:00Z"))
            .unwrap();
        tracker
            .restore("New", Some("2026-10-20T10:00:00Z"))
            .unwrap();
        tracker.observe(
            "OLD",
            &"@time=2026-10-20T12:00:00Z :b!u@h PRIVMSG me :hi"
                .parse()
                .unwrap(),
        );
        tracker.observe(
            "New",
            &"@time=2026-10-20T11:00:00Z :b!u@h PRIVMSG me :hi"
                .parse()
                .unwrap(),
        );
        tracker.rename("Old", "New").unwrap();
        assert_eq!(tracker.read("NEW"), Some("2026-10-20T10:00:00Z"));
        assert_eq!(tracker.latest("new"), Some("2026-10-20T12:00:00Z"));
        assert!(tracker.unread("new"));
        assert!(tracker.read("old").is_none());
        assert!(tracker.latest("old").is_none());
        tracker.rename("New", "NEW").unwrap();
        assert_eq!(tracker.latest("new"), Some("2026-10-20T12:00:00Z"));
        assert!(tracker.rename("new", "bad target").is_err());
        tracker.forget_latest("new");
        assert!(tracker.latest("new").is_none());
        assert_eq!(tracker.read("new"), Some("2026-10-20T10:00:00Z"));
    }
    #[test]
    fn rejects_invalid_targets_and_selector_injection() {
        for target in ["", "#one,#two", "#bad target", ":prefix", "bad\r\nMARKREAD"] {
            let message: Message = Command::Raw(
                "MARKREAD".into(),
                vec![target.into(), "timestamp=2026-10-20T10:00:00Z".into()],
            )
            .into();
            assert!(parse(&message).is_err(), "accepted {target:?}");
            assert!(
                Tracker::default()
                    .remote(Update {
                        target: target.into(),
                        time: Some("2026-10-20T10:00:00Z".into())
                    })
                    .is_err()
            );
        }
        let mut tracker = Tracker::default();
        tracker.observe(
            "#bad target",
            &"@time=2026-10-20T10:00:00Z :b!u@h PRIVMSG #test :hi"
                .parse()
                .unwrap(),
        );
        assert!(tracker.mark("#bad target").is_err());
    }
    #[test]
    fn explicit_only_marking_uses_last_chat_timestamp_and_waits_for_ack() {
        let mut t = Tracker::default();
        t.observe(
            "#test",
            &"@time=2026-10-20T10:00:00Z :b!u@h PRIVMSG #test :hi"
                .parse()
                .unwrap(),
        );
        assert!(t.unread("#test"));
        assert_eq!(
            t.mark("#test").unwrap().to_string(),
            "MARKREAD #test timestamp=2026-10-20T10:00:00Z\r\n"
        );
        assert!(t.unread("#test"));
        t.remote(
            parse(
                &"MARKREAD #test timestamp=2026-10-20T10:00:00Z"
                    .parse()
                    .unwrap(),
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        assert!(!t.unread("#test"));
        assert!(
            !t.remote(Update {
                target: "#test".into(),
                time: None
            })
            .unwrap()
        );
        assert!(
            !t.remote(Update {
                target: "#test".into(),
                time: Some("2026-10-20T11:00:00+01:00".into())
            })
            .unwrap()
        );
        assert!(t.mark("unknown").is_err());
        assert!(parse(&"MARKREAD #test bad".parse().unwrap()).is_err());
    }
}
