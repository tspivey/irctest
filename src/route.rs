//! Decides which tab an incoming message belongs in.

use irc_proto::{ChannelExt, Command, Message, Response};

use crate::format::parse_ctcp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The server tab.
    Server,
    /// This channel's tab, created if needed.
    Channel(String),
    /// A query tab for this nick, created if needed.
    Query(String),
    /// The channel or query tab with this name if one is open, otherwise the server tab.
    TabIfOpen(String),
    /// Every channel tab this nick is in, plus their query tab (QUIT, NICK).
    /// When the nick is ours: every tab of the server.
    UserTabs(String),
}

/// `me` is our current nick on this server.
pub fn route(msg: &Message, me: &str) -> Route {
    let source = msg.source_nickname();
    match &msg.command {
        Command::PRIVMSG(target, text) => {
            if target.is_channel_name() {
                return Route::Channel(target.clone());
            }
            let Some(source) = source else { return Route::Server };
            match parse_ctcp(text) {
                Some((ctcp, _)) if ctcp != "ACTION" => Route::Server,
                // Our own message (echoed by a bouncer) belongs with whoever we sent it to.
                _ if irc_eq(source, me) => Route::Query(target.clone()),
                _ => Route::Query(source.to_string()),
            }
        }
        Command::NOTICE(target, _) => {
            if target.is_channel_name() {
                return Route::TabIfOpen(target.clone());
            }
            match source {
                Some(source) if irc_eq(source, me) => Route::TabIfOpen(target.clone()),
                Some(source) => Route::TabIfOpen(source.to_string()),
                None => Route::Server,
            }
        }
        Command::JOIN(channel, _, _) => Route::Channel(channel.clone()),
        Command::PART(channel, _)
        | Command::KICK(channel, _, _)
        | Command::TOPIC(channel, _)
        | Command::ChannelMODE(channel, _) => Route::TabIfOpen(channel.clone()),
        Command::QUIT(_) | Command::NICK(_) => match source {
            Some(source) => Route::UserTabs(source.to_string()),
            None => Route::Server,
        },
        Command::Response(Response::RPL_NAMREPLY, args) => match args.get(2) {
            Some(channel) => Route::TabIfOpen(channel.clone()),
            None => Route::Server,
        },
        // /LIST output is about many channels; keep it together in the server tab.
        Command::Response(Response::RPL_LIST, _) => Route::Server,
        // WHO and WHOIS/WHOWAS output stays together in the server tab: a WHO line names whichever
        // channel the user happens to be in, and the closing numeric names the mask.
        Command::Response(
            Response::RPL_WHOREPLY
            | Response::RPL_ENDOFWHO
            | Response::RPL_WHOISUSER
            | Response::RPL_WHOISSERVER
            | Response::RPL_WHOISOPERATOR
            | Response::RPL_WHOISIDLE
            | Response::RPL_WHOISCHANNELS
            | Response::RPL_WHOISCERTFP
            | Response::RPL_WHOISKEYVALUE
            | Response::RPL_ENDOFWHOIS
            | Response::RPL_WHOWASUSER
            | Response::RPL_ENDOFWHOWAS,
            _,
        ) => Route::Server,
        // These name the nick we tried, not someone we're talking to.
        Command::Response(Response::ERR_NICKNAMEINUSE | Response::ERR_ERRONEOUSNICKNAME, _) => Route::Server,
        // `<me> <nick> <channel> ...`: about the channel, not the nick.
        Command::Response(
            Response::RPL_INVITING | Response::ERR_USERNOTINCHANNEL | Response::ERR_USERONCHANNEL,
            args,
        ) => match args.get(2) {
            Some(channel) if channel.is_channel_name() => Route::TabIfOpen(channel.clone()),
            _ => Route::Server,
        },
        // Numerics about a channel or nick (topic, no such nick, ...) go to its tab if open.
        Command::Response(_, args) => match args.get(1) {
            Some(name) if args.len() > 2 => Route::TabIfOpen(name.clone()),
            _ => Route::Server,
        },
        _ => Route::Server,
    }
}

/// IRC's case-insensitive comparison (RFC 1459 casemapping).
pub fn irc_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.chars().zip(b.chars()).all(|(x, y)| fold(x) == fold(y))
}

/// Lowercases with RFC 1459 casemapping, for use as a map key.
pub fn irc_lower(s: &str) -> String {
    s.chars().map(fold).collect()
}

fn fold(c: char) -> char {
    match c {
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        '~' => '^',
        c => c.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(line: &str) -> Route {
        route(&line.parse::<Message>().unwrap(), "tyler")
    }

    #[test]
    fn privmsgs() {
        assert_eq!(r(":bob!b@h PRIVMSG #test :hi\r\n"), Route::Channel("#test".into()));
        assert_eq!(r(":bob!b@h PRIVMSG tyler :hi\r\n"), Route::Query("bob".into()));
        assert_eq!(r(":bob!b@h PRIVMSG tyler :\x01ACTION waves\x01\r\n"), Route::Query("bob".into()));
        assert_eq!(r(":bob!b@h PRIVMSG tyler :\x01VERSION\x01\r\n"), Route::Server);
        assert_eq!(r(":Tyler!t@h PRIVMSG bob :sent elsewhere\r\n"), Route::Query("bob".into()));
        assert_eq!(r(":irc.example.net PRIVMSG tyler :hi\r\n"), Route::Server);
    }

    #[test]
    fn notices() {
        assert_eq!(r(":bob!b@h NOTICE #test :hi\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":NickServ!s@h NOTICE tyler :identify\r\n"), Route::TabIfOpen("NickServ".into()));
        assert_eq!(r(":irc.example.net NOTICE tyler :hi\r\n"), Route::Server);
        assert_eq!(r("NOTICE AUTH :hi\r\n"), Route::Server);
    }

    #[test]
    fn membership() {
        assert_eq!(r(":bob!b@h JOIN #test\r\n"), Route::Channel("#test".into()));
        assert_eq!(r(":bob!b@h PART #test :bye\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":op!o@h KICK #test bob\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":bob!b@h QUIT :bye\r\n"), Route::UserTabs("bob".into()));
        assert_eq!(r(":bob!b@h NICK bobby\r\n"), Route::UserTabs("bob".into()));
        assert_eq!(r(":op!o@h MODE #test +o bob\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":tyler MODE tyler :+i\r\n"), Route::Server);
    }

    #[test]
    fn numerics() {
        assert_eq!(r(":srv 001 tyler :Welcome\r\n"), Route::Server);
        assert_eq!(r(":srv 332 tyler #test :the topic\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":srv 353 tyler = #test :tyler bob\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":srv 401 tyler bob :No such nick\r\n"), Route::TabIfOpen("bob".into()));
        assert_eq!(r(":srv 999 tyler some :thing\r\n"), Route::Server);
        assert_eq!(r(":srv 322 tyler #test 1 :the topic\r\n"), Route::Server);
        assert_eq!(r(":srv 352 tyler #test b host srv bob H :0 Bob\r\n"), Route::Server);
        assert_eq!(r(":srv 315 tyler bob :End of WHO list\r\n"), Route::Server);
        assert_eq!(r(":srv 311 tyler bob b host * :Bob\r\n"), Route::Server);
        assert_eq!(r(":srv 319 tyler bob :#test #rust\r\n"), Route::Server);
        assert_eq!(r(":srv 318 tyler bob :End of WHOIS list\r\n"), Route::Server);
        assert_eq!(r(":srv 433 tyler bob :Nickname is already in use\r\n"), Route::Server);
        assert_eq!(r(":srv 432 tyler b@d :Erroneous nickname\r\n"), Route::Server);
        assert_eq!(r(":srv 341 tyler bob #test\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":srv 441 tyler bob #test :They aren't on that channel\r\n"), Route::TabIfOpen("#test".into()));
        assert_eq!(r(":srv 443 tyler bob #test :is already on channel\r\n"), Route::TabIfOpen("#test".into()));
        // Still goes to the nick's tab.
        assert_eq!(r(":srv 301 tyler bob :away\r\n"), Route::TabIfOpen("bob".into()));
    }

    #[test]
    fn casemapping() {
        assert!(irc_eq("Tyler[1]", "tyler{1}"));
        assert!(irc_eq("a\\b~", "A|B^"));
        assert!(!irc_eq("tyler", "tyler_"));
        assert_eq!(irc_lower("#Test[X]"), "#test{x}");
    }
}
