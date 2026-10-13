//! One tokio task per server connection, wrapping `irc::client::Client`.
//!
//! The task reports what happens through an event sink; the caller sends with the
//! `irc::client::Sender` handed over in [`NetEvent::Connected`].

use std::sync::Arc;

use futures::StreamExt;
use irc::client::prelude::{Client, Command, Config as IrcConfig, Message, Response, Sender};
use irc::error::Error as IrcError;
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use crate::config::{Config, ServerConfig};

/// Identifies a connection; chosen by the caller.
pub type ServerId = u32;

#[derive(Debug)]
pub enum NetEvent {
    /// TCP (and TLS) is up and registration has been sent. Use the sender for everything else.
    Connected(Sender),
    Message(Message),
    /// Our nick changed (alternative nick on registration, or a NICK we sent).
    NickChanged(String),
    /// Something went wrong but the connection is still up (e.g. an unparseable line).
    Error(String),
    /// The connection is gone, or never came up. Always the last event.
    Disconnected(String),
}

pub type EventSink = Arc<dyn Fn(ServerId, NetEvent) + Send + Sync>;

/// Starts connecting in the background. Abort the returned handle to drop the connection
/// without a QUIT (e.g. while a connect is hanging); otherwise send QUIT through the sender.
pub fn connect(handle: &Handle, id: ServerId, config: &Config, server: &ServerConfig, sink: EventSink) -> AbortHandle {
    let irc_config = irc_config(config, server);
    let connect_commands = server.connect_commands.clone();
    handle
        .spawn(async move {
            let reason = match irc_config {
                Ok(cfg) => run(id, cfg, connect_commands, &sink).await,
                Err(e) => e,
            };
            sink(id, NetEvent::Disconnected(reason));
        })
        .abort_handle()
}

/// Builds the irc crate's config from ours. Errors are user-facing messages.
pub fn irc_config(config: &Config, server: &ServerConfig) -> Result<IrcConfig, String> {
    let nick = config.effective_nick(server);
    if nick.is_empty() {
        return Err("No nickname set. Set a global nickname or one for this server.".into());
    }
    if server.host.trim().is_empty() {
        return Err("No host set for this server.".into());
    }
    Ok(IrcConfig {
        server: Some(server.host.trim().to_string()),
        port: Some(server.port),
        use_tls: Some(server.tls),
        password: Some(server.password.clone()),
        nickname: Some(nick.to_string()),
        alt_nicks: (1..=3).map(|n| format!("{nick}{}", "_".repeat(n))).collect(),
        username: Some(config.effective_username(server).to_string()),
        realname: Some(config.effective_realname(server).to_string()),
        encoding: Some("UTF-8".into()),
        version: Some(format!("irctest {}", env!("CARGO_PKG_VERSION"))),
        ..IrcConfig::default()
    })
}

/// Runs one connection until it ends; returns the reason.
async fn run(id: ServerId, cfg: IrcConfig, connect_commands: Vec<String>, sink: &EventSink) -> String {
    let mut client = match Client::from_config(cfg).await {
        Ok(c) => c,
        Err(e) => return format!("Couldn't connect: {e}"),
    };
    if let Err(e) = client.identify() {
        return format!("Couldn't register: {e}");
    }
    let mut stream = match client.stream() {
        Ok(s) => s,
        Err(e) => return e.to_string(),
    };
    sink(id, NetEvent::Connected(client.sender()));

    let mut nick = client.current_nickname().to_string();
    sink(id, NetEvent::NickChanged(nick.clone()));

    while let Some(item) = stream.next().await {
        match item {
            Ok(msg) => {
                let welcome = matches!(msg.command, Command::Response(Response::RPL_WELCOME, _));
                sink(id, NetEvent::Message(msg));

                if client.current_nickname() != nick {
                    nick = client.current_nickname().to_string();
                    sink(id, NetEvent::NickChanged(nick.clone()));
                }
                if welcome {
                    for line in &connect_commands {
                        if let Err(e) = send_raw(&client.sender(), line) {
                            sink(id, NetEvent::Error(format!("Connect command {line:?}: {e}")));
                        }
                    }
                }
            }
            // A line we couldn't parse isn't fatal; anything else is.
            Err(IrcError::InvalidMessage { string, cause }) => {
                sink(id, NetEvent::Error(format!("Couldn't parse {:?}: {cause}", string.trim_end())));
            }
            Err(IrcError::NoUsableNick) => return "Every nickname is in use.".into(),
            Err(e) => return e.to_string(),
        }
    }
    "Connection closed.".into()
}

/// Sends a raw line as typed by the user (`PRIVMSG #chan :hi there`).
/// Parsing it first means the irc crate re-serialises it correctly.
pub fn send_raw(sender: &Sender, line: &str) -> Result<(), String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let msg: Message = line.parse().map_err(|e| format!("{e}"))?;
    sender.send(msg).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> ServerConfig {
        ServerConfig { name: "lan".into(), host: "10.0.0.2".into(), ..ServerConfig::default() }
    }

    #[test]
    fn irc_config_uses_effective_names() {
        let config = Config { nickname: "tyler".into(), ..Config::default() };
        let cfg = irc_config(&config, &server()).unwrap();
        assert_eq!(cfg.nickname.as_deref(), Some("tyler"));
        assert_eq!(cfg.alt_nicks, vec!["tyler_", "tyler__", "tyler___"]);
        assert_eq!(cfg.username.as_deref(), Some("tyler"));
        assert_eq!(cfg.server.as_deref(), Some("10.0.0.2"));
        assert_eq!(cfg.port, Some(6667));
        assert!(cfg.channels.is_empty());
    }

    #[test]
    fn irc_config_needs_nick_and_host() {
        assert!(irc_config(&Config::default(), &server()).is_err());
        let config = Config { nickname: "tyler".into(), ..Config::default() };
        assert!(irc_config(&config, &ServerConfig::default()).is_err());
    }

    #[test]
    fn raw_lines_parse() {
        let msg: Message = "PRIVMSG #test :hello there".parse().unwrap();
        assert_eq!(msg.to_string(), "PRIVMSG #test :hello there\r\n");
        let msg: Message = "JOIN #test".parse().unwrap();
        assert_eq!(msg.to_string(), "JOIN #test\r\n");
    }
}
