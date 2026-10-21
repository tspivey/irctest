//! Exact IRC nickname/channel ignores, shared by live display and playback.

use irc_proto::{ChannelExt, Command, Message};

use crate::route::irc_eq;

pub fn contains(entries: &[String], name: &str) -> bool {
    entries.iter().any(|entry| irc_eq(entry.trim(), name))
}

/// Trim input, reject lists and invalid control characters, and avoid duplicates.
pub fn add(entries: &mut Vec<String>, name: &str) -> Result<(), &'static str> {
    let name = name.trim();
    if name.is_empty()
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ',')
    {
        return Err("Enter one nickname or channel name, without spaces or commas.");
    }
    if !contains(entries, name) {
        entries.push(name.to_owned());
    }
    Ok(())
}

/// Filter user text before routing, so ignored private messages cannot open tabs.
/// Membership processing still runs for ignored users' JOIN/PART/NICK events.
pub fn message_ignored(entries: &[String], message: &Message) -> bool {
    if message
        .source_nickname()
        .is_some_and(|nick| contains(entries, nick))
    {
        return true;
    }
    match &message.command {
        Command::PRIVMSG(target, _) | Command::NOTICE(target, _) => {
            target.is_channel_name() && contains(entries, target)
        }
        _ => false,
    }
}

/// Reject the whole send when any recipient is ignored, including comma-separated
/// recipients and channel status targets such as `@#channel`.
pub fn outgoing_ignored(entries: &[String], command: &Command) -> bool {
    let targets = match command {
        Command::PRIVMSG(targets, _) | Command::NOTICE(targets, _) => targets,
        // The IRC parser preserves extra/missing parameters as a raw command.
        // A server may still deliver it, so check its recipient before sending.
        Command::Raw(command, args)
            if command.eq_ignore_ascii_case("PRIVMSG")
                || command.eq_ignore_ascii_case("NOTICE") =>
        {
            let Some(targets) = args.first() else {
                return false;
            };
            targets
        }
        _ => return false,
    };
    targets.split(',').any(|target| {
        if contains(entries, target) {
            return true;
        }
        // Test after each prefix so @&local still matches the channel &local.
        for (offset, prefix) in target.char_indices() {
            if !['~', '&', '@', '%', '+'].contains(&prefix) {
                break;
            }
            let channel = &target[offset + prefix.len_utf8()..];
            if channel.is_channel_name() && contains(entries, channel) {
                return true;
            }
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_messages_block_any_ignored_recipient_and_status_channel() {
        let entries = vec!["Bob".into(), "#quiet[1]".into(), "&local".into()];
        for line in [
            "PRIVMSG bOB :private",
            "NOTICE BOB :notice",
            "PRIVMSG Alice,bOB :multiple recipients",
            "NOTICE #other,#QUIET{1} :multiple channels",
            "PRIVMSG @#quiet[1] :operators",
            "NOTICE @+#QUIET{1} :status prefixes",
            "PRIVMSG @&LOCAL :local channel operators",
            "PRIVMSG Bob :\x01ACTION waves\x01",
            "PRIVMSG Bob :\x01VERSION\x01",
            "PRIVMSG Bob hello there",
            "notice #QUIET{1} hello there",
        ] {
            let message: Message = line.parse().unwrap();
            assert!(
                outgoing_ignored(&entries, &message.command),
                "allowed {line}"
            );
        }
        for line in [
            "PRIVMSG Bobby :different nickname",
            "PRIVMSG &Bob :different channel",
            "PRIVMSG #other :Bob is mentioned",
            "PRIVMSG Alice,#other :allowed recipients",
            "NOTICE Alice :allowed notice",
            "JOIN #quiet[1]",
            "PART #quiet[1]",
            "NICK Bob",
        ] {
            let message: Message = line.parse().unwrap();
            assert!(
                !outgoing_ignored(&entries, &message.command),
                "blocked {line}"
            );
        }
    }

    #[test]
    fn entries_are_trimmed_and_deduplicated_with_irc_casemapping() {
        let mut entries = Vec::new();
        add(&mut entries, "  Bob[away]  ").unwrap();
        add(&mut entries, "bOB{away}").unwrap();
        assert_eq!(entries, ["Bob[away]"]);
        assert!(contains(&entries, "BOB{away}"));
        assert!(!contains(&entries, "Bob"));
        for invalid in ["", " ", "bob alice", "bob,alice", "bob\nalice", "bob\0"] {
            assert!(add(&mut entries, invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn filters_private_channel_action_and_notice_text_without_substring_matches() {
        let entries = vec!["Bob".into(), "#quiet[1]".into()];
        for line in [
            ":bOB!u@h PRIVMSG me :private",
            ":Bob!u@h PRIVMSG #other :channel",
            ":Bob!u@h NOTICE me :notice",
            ":Bob!u@h PRIVMSG #other :\x01ACTION waves\x01",
            ":Alice!u@h PRIVMSG #QUIET{1} :channel",
            ":Alice!u@h NOTICE #quiet[1] :notice",
        ] {
            assert!(
                message_ignored(&entries, &line.parse().unwrap()),
                "leaked {line}"
            );
        }
        for line in [
            ":Bobby!u@h PRIVMSG me :private",
            ":me!u@h PRIVMSG Bob :our outgoing private message",
            ":Alice!u@h PRIVMSG #other :Bob is here",
            ":Alice!u@h PRIVMSG #quiet[1]extra :different channel",
            ":srv 001 me :Welcome Bob",
            "PING :server",
        ] {
            assert!(
                !message_ignored(&entries, &line.parse().unwrap()),
                "hid {line}"
            );
        }
    }
}
