//! Parses what's typed in an input box. Executing it is up to the GUI.

use irc_proto::ChannelExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Plain text (or `//text` for text starting with `/`): PRIVMSG to the tab's channel/nick.
    Say(String),
    /// `/me action`
    Me(String),
    /// `/msg target text`
    Msg { target: String, text: String },
    /// `/join channels [keys]`, sent as `JOIN <args>`.
    Join(String),
    /// `/part [#channel] [reason]`; no channel means the current tab's.
    Part { channel: Option<String>, reason: Option<String> },
    /// `/query nick [text]`
    Query { nick: String, text: Option<String> },
    /// `/nick newnick`
    Nick(String),
    /// `/quit [reason]`
    Quit(Option<String>),
    /// `/close`: part (for a channel) and remove the tab.
    Close,
    /// `/raw line`, `/quote line`, or any other `/command args`.
    Raw(String),
    /// The command was recognised but used wrongly; the text says how to use it.
    Usage(&'static str),
}

/// `None` for blank input.
pub fn parse_input(line: &str) -> Option<Input> {
    if line.trim().is_empty() {
        return None;
    }
    let Some(command_line) = line.strip_prefix('/') else {
        return Some(Input::Say(line.to_string()));
    };
    if command_line.starts_with('/') {
        return Some(Input::Say(command_line.to_string()));
    }

    let (command, rest) = split_word(command_line);
    let rest = rest.trim();
    let input = match command.to_ascii_lowercase().as_str() {
        "me" => Input::Me(rest.to_string()),
        "msg" => match split_word(rest) {
            (target, text) if !target.is_empty() && !text.is_empty() => {
                Input::Msg { target: target.to_string(), text: text.to_string() }
            }
            _ => Input::Usage("Usage: /msg nick-or-channel message"),
        },
        "join" | "j" if rest.is_empty() => Input::Usage("Usage: /join #channel [key]"),
        "join" | "j" => Input::Join(rest.to_string()),
        "part" | "leave" => {
            let (first, after) = split_word(rest);
            if first.is_channel_name() {
                Input::Part { channel: Some(first.to_string()), reason: non_empty(after) }
            } else {
                Input::Part { channel: None, reason: non_empty(rest) }
            }
        }
        "query" | "q" => match split_word(rest) {
            ("", _) => Input::Usage("Usage: /query nick [message]"),
            (nick, text) => Input::Query { nick: nick.to_string(), text: non_empty(text) },
        },
        "nick" => match split_word(rest) {
            ("", _) => Input::Usage("Usage: /nick newnick"),
            (nick, _) => Input::Nick(nick.to_string()),
        },
        "quit" => Input::Quit(non_empty(rest)),
        "close" => Input::Close,
        "raw" | "quote" if rest.is_empty() => Input::Usage("Usage: /raw IRC command"),
        "raw" | "quote" => Input::Raw(rest.to_string()),
        _ => Input::Raw(command_line.trim().to_string()),
    };
    Some(input)
}

/// Splits off the first word; the rest keeps its inner spacing.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.split_once(' ') {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (text, ""),
    }
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Input {
        parse_input(line).unwrap()
    }

    #[test]
    fn blank_is_nothing() {
        assert_eq!(parse_input(""), None);
        assert_eq!(parse_input("   "), None);
    }

    #[test]
    fn plain_text_and_escaped_slash() {
        assert_eq!(p("hello there"), Input::Say("hello there".into()));
        assert_eq!(p("//shrug"), Input::Say("/shrug".into()));
    }

    #[test]
    fn me_and_msg() {
        assert_eq!(p("/me waves  hello"), Input::Me("waves  hello".into()));
        assert_eq!(p("/msg bob hi  there"), Input::Msg { target: "bob".into(), text: "hi  there".into() });
        assert_eq!(p("/MSG #test hi"), Input::Msg { target: "#test".into(), text: "hi".into() });
        assert!(matches!(p("/msg bob"), Input::Usage(_)));
    }

    #[test]
    fn join_and_part() {
        assert_eq!(p("/join #a,#b key"), Input::Join("#a,#b key".into()));
        assert_eq!(p("/j #a"), Input::Join("#a".into()));
        assert!(matches!(p("/join"), Input::Usage(_)));
        assert_eq!(p("/part"), Input::Part { channel: None, reason: None });
        assert_eq!(p("/part bye all"), Input::Part { channel: None, reason: Some("bye all".into()) });
        assert_eq!(p("/part #x see you"), Input::Part { channel: Some("#x".into()), reason: Some("see you".into()) });
    }

    #[test]
    fn query_nick_quit_close() {
        assert_eq!(p("/query bob"), Input::Query { nick: "bob".into(), text: None });
        assert_eq!(p("/q bob hi"), Input::Query { nick: "bob".into(), text: Some("hi".into()) });
        assert!(matches!(p("/query"), Input::Usage(_)));
        assert_eq!(p("/nick newme"), Input::Nick("newme".into()));
        assert_eq!(p("/quit"), Input::Quit(None));
        assert_eq!(p("/quit gone fishing"), Input::Quit(Some("gone fishing".into())));
        assert_eq!(p("/close"), Input::Close);
    }

    #[test]
    fn raw_and_unknown() {
        assert_eq!(p("/raw PRIVMSG #a :hi"), Input::Raw("PRIVMSG #a :hi".into()));
        assert_eq!(p("/quote MODE #a"), Input::Raw("MODE #a".into()));
        assert_eq!(p("/whois bob"), Input::Raw("whois bob".into()));
        assert!(matches!(p("/raw"), Input::Usage(_)));
    }
}
