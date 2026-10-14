//! Turns IRC messages into display lines and speech text.
//!
//! Which tab a line goes to, timestamps, and whether to actually speak are decided by the
//! caller; this module only produces the text.

use chrono::{DateTime, Local, TimeZone};
use irc_proto::{ChannelExt, Command, Message, Prefix, Response};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatted {
    /// The line for the output box, without timestamp.
    pub text: String,
    /// What to say if speech is on for the tab. Only set for PRIVMSGs (including ACTIONs).
    pub speech: Option<String>,
}

impl Formatted {
    fn line(text: String) -> Self {
        Self { text: strip_formatting(&text), speech: None }
    }

    fn spoken(text: String, speech: String) -> Self {
        Self { text: strip_formatting(&text), speech: Some(strip_formatting(&speech)) }
    }
}

/// Formats an incoming message. `None` for messages not worth showing (PING, PONG, CAP, empty lines).
pub fn format_message(msg: &Message) -> Option<Formatted> {
    let nick = msg.source_nickname().unwrap_or("");
    let source = source_name(msg);

    let formatted = match &msg.command {
        Command::PRIVMSG(target, text) => match parse_ctcp(text) {
            Some(("ACTION", action)) => {
                let speech = if target.is_channel_name() {
                    format!("{nick} on {} {action}", spoken_channel(target))
                } else {
                    format!("{nick} {action}")
                };
                Formatted::spoken(format!("* {nick} {action}"), speech)
            }
            Some((ctcp, args)) => Formatted::line(join_nonempty(&format!("CTCP {ctcp} from {source}"), args)),
            None => {
                let speech = if target.is_channel_name() {
                    format!("{nick} on {} says {text}", spoken_channel(target))
                } else {
                    format!("{nick} says {text}")
                };
                Formatted::spoken(format!("<{nick}> {text}"), speech)
            }
        },
        Command::NOTICE(_, text) => match parse_ctcp(text) {
            Some((ctcp, args)) => Formatted::line(join_nonempty(&format!("CTCP {ctcp} reply from {source}:"), args)),
            None => Formatted::line(format!("-{source}- {text}")),
        },
        Command::JOIN(channel, _, _) => Formatted::line(format!("{nick} has joined {channel}")),
        Command::PART(channel, reason) => {
            Formatted::line(with_reason(format!("{nick} has left {channel}"), reason.as_deref()))
        }
        Command::QUIT(reason) => Formatted::line(with_reason(format!("{nick} has quit"), reason.as_deref())),
        Command::NICK(new) => Formatted::line(format!("{nick} is now known as {new}")),
        Command::TOPIC(channel, Some(topic)) if topic.is_empty() => {
            Formatted::line(format!("{source} cleared the topic of {channel}"))
        }
        Command::TOPIC(channel, Some(topic)) => {
            Formatted::line(format!("{source} changed the topic of {channel} to: {topic}"))
        }
        Command::KICK(channel, victim, reason) => Formatted::line(with_reason(
            format!("{source} has kicked {victim} from {channel}"),
            reason.as_deref(),
        )),
        Command::ChannelMODE(channel, modes) => {
            let modes: Vec<String> = modes.iter().map(|m| m.to_string()).collect();
            Formatted::line(format!("{source} sets mode {} on {channel}", modes.join(" ")))
        }
        Command::UserMODE(target, modes) => {
            let modes: Vec<String> = modes.iter().map(|m| m.to_string()).collect();
            Formatted::line(format!("{source} sets mode {} on {target}", modes.join(" ")))
        }
        Command::INVITE(_, channel) => Formatted::line(format!("{source} invites you to {channel}")),
        Command::ERROR(text) => Formatted::line(format!("Error: {text}")),
        Command::PING(..) | Command::PONG(..) | Command::CAP(..) | Command::AUTHENTICATE(_) => return None,
        // Empty or prefix-only lines parse as an empty raw command.
        Command::Raw(cmd, _) if cmd.is_empty() => return None,
        // Numerics: the first parameter is our own nick, so leave it out.
        Command::Response(Response::RPL_NAMREPLY, args) if args.len() >= 4 => {
            Formatted::line(format!("Users on {}: {}", args[2], args[3]))
        }
        Command::Response(Response::RPL_ENDOFNAMES, _) => return None,
        Command::Response(Response::RPL_NOTOPIC, args) if args.len() >= 2 => {
            Formatted::line(format!("No topic is set for {}", args[1]))
        }
        Command::Response(Response::RPL_TOPIC, args) if args.len() >= 3 => {
            Formatted::line(format!("Topic for {}: {}", args[1], args[2]))
        }
        Command::Response(Response::RPL_TOPICWHOTIME, args) if args.len() >= 3 => {
            let setter = args[2].split('!').next().unwrap_or(&args[2]);
            let when = args
                .get(3)
                .and_then(|t| t.parse::<i64>().ok())
                .and_then(|t| Local.timestamp_opt(t, 0).single())
                .map(|t| format!(" on {}", t.format("%Y-%m-%d %H:%M")))
                .unwrap_or_default();
            Formatted::line(format!("Topic for {} set by {setter}{when}", args[1]))
        }
        // `<nick> <mask> [<account>] :You are now logged in as ...`: the text says it all.
        Command::Response(Response::RPL_LOGGEDIN | Response::RPL_LOGGEDOUT, args) if !args.is_empty() => {
            Formatted::line(args[args.len() - 1].clone())
        }
        Command::Response(_, args) => Formatted::line(args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ")),
        Command::Raw(cmd, args) if is_numeric(cmd) => {
            Formatted::line(args.iter().skip(1).cloned().collect::<Vec<_>>().join(" "))
        }
        // Anything else: show the raw line without tags.
        _ => {
            let bare = Message { tags: None, prefix: msg.prefix.clone(), command: msg.command.clone() };
            Formatted::line(bare.to_string().trim_end().to_string())
        }
    };
    Some(formatted)
}

/// Nick for users, server name for servers, `*` if there's no prefix.
fn source_name(msg: &Message) -> &str {
    match &msg.prefix {
        Some(Prefix::Nickname(nick, _, _)) => nick,
        Some(Prefix::ServerName(name)) => name,
        None => "*",
    }
}

/// When the message was sent, from its IRCv3 `time` tag (`server-time`), if it has one.
pub fn message_time(msg: &Message) -> Option<DateTime<Local>> {
    msg.tags
        .as_ref()?
        .iter()
        .find(|tag| tag.0 == "time")
        .and_then(|tag| tag.1.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Local))
}

/// `"[HH:MM] text"`, in local time.
pub fn timestamped(time: DateTime<Local>, text: &str) -> String {
    format!("[{}] {text}", time.format("%H:%M"))
}

/// A channel name as it should be spoken: without its leading `#`s.
pub fn spoken_channel(channel: &str) -> &str {
    let trimmed = channel.trim_start_matches('#');
    if trimmed.is_empty() { channel } else { trimmed }
}

/// Splits `\x01COMMAND args\x01` into `("COMMAND", "args")`. The closing `\x01` is optional,
/// as some clients leave it off.
pub fn parse_ctcp(text: &str) -> Option<(&str, &str)> {
    let inner = text.strip_prefix('\x01')?;
    let inner = inner.strip_suffix('\x01').unwrap_or(inner);
    match inner.split_once(' ') {
        Some((cmd, args)) => Some((cmd, args)),
        None => Some((inner, "")),
    }
}

fn is_numeric(cmd: &str) -> bool {
    cmd.len() == 3 && cmd.bytes().all(|b| b.is_ascii_digit())
}

fn with_reason(text: String, reason: Option<&str>) -> String {
    match reason {
        Some(r) if !r.is_empty() => format!("{text} ({r})"),
        _ => text,
    }
}

fn join_nonempty(head: &str, tail: &str) -> String {
    if tail.is_empty() { head.to_string() } else { format!("{head} {tail}") }
}

/// Removes mIRC formatting: bold, italics, underline, strikethrough, monospace, reverse,
/// reset, `\x03` colours (`fg[,bg]`, 1-2 digits each) and `\x04` hex colours (6 hex digits each).
///
/// Written here rather than using `irc_proto::FormattedStringExt`, which misses italics,
/// strikethrough and monospace and mishandles some two-digit colour codes.
pub fn strip_formatting(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x02' | '\x0f' | '\x11' | '\x16' | '\x1d' | '\x1e' | '\x1f' => {}
            '\x03' => skip_colour(&mut chars, 2, |c| c.is_ascii_digit()),
            '\x04' => skip_colour(&mut chars, 6, |c| c.is_ascii_hexdigit()),
            _ => out.push(c),
        }
    }
    out
}

/// Skips `fg[,bg]` after a colour code, each being up to `max` chars matching `is_digit`.
/// A comma is only consumed when a background colour follows it.
fn skip_colour(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, max: usize, is_digit: fn(char) -> bool) {
    if skip_digits(chars, max, is_digit) == 0 {
        return;
    }
    let mut lookahead = chars.clone();
    if lookahead.next() == Some(',') && lookahead.next().is_some_and(is_digit) {
        chars.next();
        skip_digits(chars, max, is_digit);
    }
}

fn skip_digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, max: usize, is_digit: fn(char) -> bool) -> usize {
    let mut n = 0;
    while n < max && chars.peek().copied().is_some_and(is_digit) {
        chars.next();
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(line: &str) -> Option<Formatted> {
        format_message(&line.parse::<Message>().unwrap())
    }

    fn text(line: &str) -> String {
        fmt(line).unwrap().text
    }

    #[test]
    fn sasl_lines() {
        assert_eq!(
            text(":irc.test 900 tyler tyler!t@host tyler :You are now logged in as tyler\r\n"),
            "You are now logged in as tyler"
        );
        assert_eq!(text(":irc.test 903 tyler :SASL authentication successful\r\n"), "SASL authentication successful");
        assert!(fmt("AUTHENTICATE +\r\n").is_none());
        assert!(fmt(":irc.test CAP * LS :sasl\r\n").is_none());
    }

    #[test]
    fn channel_message() {
        let f = fmt(":bob!b@host PRIVMSG #test :hello there\r\n").unwrap();
        assert_eq!(f.text, "<bob> hello there");
        assert_eq!(f.speech.as_deref(), Some("bob on test says hello there"));
    }

    #[test]
    fn channel_name_loses_all_leading_hashes() {
        let f = fmt(":bob!b@host PRIVMSG ##rust :hi\r\n").unwrap();
        assert_eq!(f.speech.as_deref(), Some("bob on rust says hi"));
        assert_eq!(spoken_channel("#"), "#");
    }

    #[test]
    fn private_message() {
        let f = fmt(":bob!b@host PRIVMSG tyler :psst\r\n").unwrap();
        assert_eq!(f.text, "<bob> psst");
        assert_eq!(f.speech.as_deref(), Some("bob says psst"));
    }

    #[test]
    fn action() {
        let f = fmt(":bob!b@host PRIVMSG #test :\x01ACTION waves\x01\r\n").unwrap();
        assert_eq!(f.text, "* bob waves");
        assert_eq!(f.speech.as_deref(), Some("bob on test waves"));

        let f = fmt(":bob!b@host PRIVMSG tyler :\x01ACTION waves\r\n").unwrap();
        assert_eq!(f.text, "* bob waves");
        assert_eq!(f.speech.as_deref(), Some("bob waves"));
    }

    #[test]
    fn other_ctcp_is_shown_not_spoken() {
        let f = fmt(":bob!b@host PRIVMSG tyler :\x01VERSION\x01\r\n").unwrap();
        assert_eq!(f.text, "CTCP VERSION from bob");
        assert_eq!(f.speech, None);
        assert_eq!(
            text(":bob!b@host NOTICE tyler :\x01VERSION mIRC 7\x01\r\n"),
            "CTCP VERSION reply from bob: mIRC 7"
        );
    }

    #[test]
    fn notices() {
        let f = fmt(":bob!b@host NOTICE #test :hi all\r\n").unwrap();
        assert_eq!(f.text, "-bob- hi all");
        assert_eq!(f.speech, None);
        assert_eq!(text(":irc.example.net NOTICE * :*** Looking up your hostname\r\n"), "-irc.example.net- *** Looking up your hostname");
        assert_eq!(text("NOTICE AUTH :hello\r\n"), "-*- hello");
    }

    #[test]
    fn colour_codes_stripped_from_text_and_speech() {
        let f = fmt(":bob!b@host PRIVMSG #test :\x02bold\x02 \x0304,12red\x03 plain\r\n").unwrap();
        assert_eq!(f.text, "<bob> bold red plain");
        assert_eq!(f.speech.as_deref(), Some("bob on test says bold red plain"));
    }

    #[test]
    fn strip_formatting_cases() {
        assert_eq!(strip_formatting("plain"), "plain");
        assert_eq!(strip_formatting("\x1ditalic\x1d \x1estrike\x1e \x11mono\x11 \x16rev\x0f"), "italic strike mono rev");
        assert_eq!(strip_formatting("\x0304,05text"), "text");
        assert_eq!(strip_formatting("\x0399text"), "text");
        assert_eq!(strip_formatting("\x034,text"), ",text"); // comma without background is text
        assert_eq!(strip_formatting("\x03123"), "3"); // at most two digits
        assert_eq!(strip_formatting("\x03text"), "text");
        assert_eq!(strip_formatting("\x04FF0000,00ff00hex"), "hex");
        assert_eq!(strip_formatting("héllo \x02wörld"), "héllo wörld");
    }

    #[test]
    fn membership_events() {
        assert_eq!(text(":bob!b@host JOIN #test\r\n"), "bob has joined #test");
        assert_eq!(text(":bob!b@host JOIN :#test\r\n"), "bob has joined #test");
        assert_eq!(text(":bob!b@host PART #test :bye\r\n"), "bob has left #test (bye)");
        assert_eq!(text(":bob!b@host PART #test\r\n"), "bob has left #test");
        assert_eq!(text(":bob!b@host QUIT :Ping timeout\r\n"), "bob has quit (Ping timeout)");
        assert_eq!(text(":bob!b@host NICK bobby\r\n"), "bob is now known as bobby");
        assert_eq!(text(":op!o@host KICK #test bob :spam\r\n"), "op has kicked bob from #test (spam)");
    }

    #[test]
    fn topic_and_modes() {
        assert_eq!(text(":bob!b@host TOPIC #test :new topic\r\n"), "bob changed the topic of #test to: new topic");
        assert_eq!(text(":bob!b@host TOPIC #test :\r\n"), "bob cleared the topic of #test");
        assert_eq!(text(":op!o@host MODE #test +o bob\r\n"), "op sets mode +o bob on #test");
        assert_eq!(text(":tyler MODE tyler :+i\r\n"), "tyler sets mode +i on tyler");
    }

    #[test]
    fn numerics_drop_our_nick() {
        assert_eq!(
            text(":irc.example.net 001 tyler :Welcome to the network tyler\r\n"),
            "Welcome to the network tyler"
        );
        assert_eq!(text(":irc.example.net 372 tyler :- MOTD line\r\n"), "- MOTD line");
        assert_eq!(text(":irc.example.net 999 tyler some :thing\r\n"), "some thing");
    }

    #[test]
    fn error_and_unknown() {
        assert_eq!(text("ERROR :Closing link\r\n"), "Error: Closing link");
        assert_eq!(text(":bob!b@host WALLOPS :hi opers\r\n"), ":bob!b@host WALLOPS :hi opers");
    }

    #[test]
    fn ping_pong_hidden() {
        assert_eq!(fmt("PING :irc.example.net\r\n"), None);
        assert_eq!(fmt(":irc.example.net PONG irc.example.net :x\r\n"), None);
    }

    #[test]
    fn topic_and_names_numerics() {
        assert_eq!(text(":srv 332 tyler #test :the topic\r\n"), "Topic for #test: the topic");
        assert_eq!(text(":srv 331 tyler #test :No topic is set\r\n"), "No topic is set for #test");
        assert_eq!(text(":srv 333 tyler #test bob!b@h\r\n"), "Topic for #test set by bob");
        assert!(text(":srv 333 tyler #test bob 1760356800\r\n").starts_with("Topic for #test set by bob on 20"));
        assert_eq!(text(":srv 353 tyler = #test :tyler @bob\r\n"), "Users on #test: tyler @bob");
        assert_eq!(fmt(":srv 366 tyler #test :End of NAMES list\r\n"), None);
    }

    #[test]
    fn server_time_tag() {
        let msg: Message = "@time=2026-10-13T12:34:56.000Z :bob!b@h PRIVMSG #test :hi\r\n".parse().unwrap();
        let expected = DateTime::parse_from_rfc3339("2026-10-13T12:34:56Z").unwrap().with_timezone(&Local);
        assert_eq!(message_time(&msg), Some(expected));
        assert_eq!(message_time(&":bob!b@h PRIVMSG #test :hi\r\n".parse().unwrap()), None);
        assert_eq!(timestamped(expected, "x"), format!("[{}] x", expected.format("%H:%M")));
    }

    #[test]
    fn junk_lines_hidden() {
        assert_eq!(fmt("\r\n"), None);
        assert_eq!(fmt(":onlyprefix\r\n"), None);
        assert_eq!(fmt("   \r\n"), None);
    }

    #[test]
    fn tags_are_ignored() {
        let f = fmt("@time=2026-10-13T12:00:00.000Z :bob!b@host PRIVMSG #test :hi\r\n").unwrap();
        assert_eq!(f.text, "<bob> hi");
    }
}
