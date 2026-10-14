//! One tokio task per server connection: connects (with TLS if asked), registers, answers
//! PINGs and CTCP queries, and passes every message on.
//!
//! The task reports what happens through an event sink; the caller sends with the
//! [`Sender`] handed over in [`NetEvent::Connected`].

use std::fmt;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use irc_proto::{Command, Message, Response};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{Instant, interval_at, sleep_until};

use crate::config::{Config, ServerConfig};
use crate::format::parse_ctcp;
use crate::route::irc_eq;

/// Identifies a connection; chosen by the caller.
pub type ServerId = u32;

#[derive(Debug)]
pub enum NetEvent {
    /// TCP (and TLS) is up and registration has been sent. Use the sender for everything else.
    Connected(Sender),
    Message(Message),
    /// Our nick changed (alternative nick on registration, the server's 001, or a NICK of ours).
    /// Sent after the message that caused it.
    NickChanged(String),
    /// Something went wrong but the connection is still up (e.g. an unparseable line).
    Error(String),
    /// The server's TLS certificate isn't trusted by Windows or by fingerprint. The connection
    /// waits, without registering, until the prompt is answered.
    UntrustedCertificate(CertificatePrompt),
    /// The connection is gone, or never came up. Always the last event.
    /// `retry` is false when reconnecting wouldn't help (no nick set, every nick in use,
    /// certificate refused).
    Disconnected { reason: String, retry: bool },
}

pub type EventSink = Arc<dyn Fn(ServerId, NetEvent) + Send + Sync>;

/// Asks whether to carry on with a certificate Windows doesn't trust.
#[derive(Debug)]
pub struct CertificatePrompt {
    /// SHA-256 of the certificate, as colon-separated hex.
    pub fingerprint: String,
    /// Why Windows rejected it.
    pub reason: String,
    /// A different fingerprint is saved for this server: the certificate has changed.
    pub changed: bool,
    reply: oneshot::Sender<bool>,
}

impl CertificatePrompt {
    /// True carries on with this connection; false drops it without reconnecting.
    /// Dropping the prompt unanswered is the same as false.
    pub fn answer(self, trust: bool) {
        let _ = self.reply.send(trust);
    }
}

/// Queues messages for a connection. Cheap to clone; sending never blocks.
#[derive(Debug, Clone)]
pub struct Sender(mpsc::UnboundedSender<Message>);

/// The connection a [`Sender`] belongs to has ended.
#[derive(Debug)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the connection is closed")
    }
}

impl std::error::Error for Closed {}

impl Sender {
    pub fn send(&self, msg: impl Into<Message>) -> Result<(), Closed> {
        self.0.send(msg.into()).map_err(|_| Closed)
    }

    pub fn send_privmsg(&self, target: &str, text: &str) -> Result<(), Closed> {
        self.send(Command::PRIVMSG(target.into(), text.into()))
    }

    pub fn send_action(&self, target: &str, text: &str) -> Result<(), Closed> {
        self.send_privmsg(target, &format!("\x01ACTION {text}\x01"))
    }

    /// A blank reason sends a plain `QUIT`.
    pub fn send_quit(&self, reason: &str) -> Result<(), Closed> {
        self.send(Command::QUIT((!reason.is_empty()).then(|| reason.into())))
    }
}

/// How often we PING the server, and how long we wait for any reply before giving up.
const PING_INTERVAL: Duration = Duration::from_secs(180);
const PING_TIMEOUT: Duration = Duration::from_secs(20);
/// A longer "line" is cut here and handled as a line of its own (it won't parse).
/// IRCv3 allows 8191 bytes of tags plus 512 for the rest.
const MAX_LINE: usize = 16 * 1024;

/// Everything a connection needs from the config, resolved.
#[derive(Debug, Clone)]
pub struct Registration {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// Sent as PASS when not blank.
    pub password: String,
    pub nick: String,
    /// Tried in turn if the server refuses `nick` during registration.
    pub alt_nicks: Vec<String>,
    pub username: String,
    pub realname: String,
    /// Raw lines sent after 001.
    pub connect_commands: Vec<String>,
    /// The fingerprint saved for this server; blank if none.
    pub pinned: String,
    /// Fingerprints the user accepted for this run of the program only.
    pub accepted: Vec<String>,
}

impl Registration {
    /// `accepted` are certificate fingerprints trusted for now, on top of the saved one.
    /// Errors are user-facing messages.
    pub fn new(config: &Config, server: &ServerConfig, accepted: &[String]) -> Result<Self, String> {
        let nick = config.effective_nick(server);
        if nick.is_empty() {
            return Err("No nickname set. Set a global nickname or one for this server.".into());
        }
        if server.host.trim().is_empty() {
            return Err("No host set for this server.".into());
        }
        Ok(Self {
            host: server.host.trim().to_string(),
            port: server.port,
            tls: server.tls,
            password: server.password.clone(),
            nick: nick.to_string(),
            alt_nicks: (1..=3).map(|n| format!("{nick}{}", "_".repeat(n))).collect(),
            username: config.effective_username(server).to_string(),
            realname: config.effective_realname(server).to_string(),
            connect_commands: server.connect_commands.clone(),
            pinned: server.fingerprint.trim().to_string(),
            accepted: accepted.to_vec(),
        })
    }

    fn trusts(&self, fingerprint: &str) -> bool {
        std::iter::once(&self.pinned).chain(&self.accepted).any(|f| same_fingerprint(f, fingerprint))
    }
}

/// Starts connecting in the background. Abort the returned handle to drop the connection
/// without a QUIT (e.g. while a connect is hanging); otherwise send QUIT through the sender.
/// `accepted` are certificate fingerprints trusted for now, on top of the saved one.
pub fn connect(
    handle: &Handle,
    id: ServerId,
    config: &Config,
    server: &ServerConfig,
    accepted: &[String],
    sink: EventSink,
) -> AbortHandle {
    let registration = Registration::new(config, server, accepted);
    handle
        .spawn(async move {
            let (reason, retry) = match registration {
                Ok(reg) => run(id, reg, sink.clone()).await,
                Err(e) => (e, false),
            };
            sink(id, NetEvent::Disconnected { reason, retry });
        })
        .abort_handle()
}

/// Runs one connection until it ends; returns the reason and whether a reconnect makes sense.
async fn run(id: ServerId, reg: Registration, sink: EventSink) -> (String, bool) {
    let tcp = match tcp_connect(&reg).await {
        Ok(tcp) => tcp,
        Err(end) => return end,
    };
    if !reg.tls {
        return session(id, reg, tcp, sink).await;
    }
    let tls = match tls_connect(&reg.host, tcp, true).await {
        Ok(tls) => tls,
        Err(e) => match check_certificate(id, &reg, &sink, describe(&e)).await {
            Ok(tls) => tls,
            Err(end) => return end,
        },
    };
    session(id, reg, tls, sink).await
}

async fn tcp_connect(reg: &Registration) -> Result<TcpStream, (String, bool)> {
    TcpStream::connect((reg.host.as_str(), reg.port)).await.map_err(|e| (format!("Couldn't connect: {}", describe(&e)), true))
}

type TlsStream = tokio_native_tls::TlsStream<TcpStream>;

/// A TLS handshake; with `verify` false any certificate is accepted.
async fn tls_connect(host: &str, tcp: TcpStream, verify: bool) -> Result<TlsStream, native_tls::Error> {
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(!verify)
        .danger_accept_invalid_hostnames(!verify)
        .build()?;
    tokio_native_tls::TlsConnector::from(connector).connect(host, tcp).await
}

/// The handshake failed with `reason`, perhaps over the certificate. Connects again without
/// checking it, and uses that connection if its certificate's fingerprint is trusted or the
/// user says to go ahead. Checking the connection we then use (not an earlier one) means a
/// server can't show one certificate to the check and another to the session.
async fn check_certificate(id: ServerId, reg: &Registration, sink: &EventSink, reason: String) -> Result<TlsStream, (String, bool)> {
    let failed = || (format!("TLS handshake failed: {reason}"), true);
    // Any failure here means the problem wasn't (only) the certificate: report the first error.
    let tcp = tcp_connect(reg).await?;
    let tls = tls_connect(&reg.host, tcp, false).await.map_err(|_| failed())?;
    let der = match tls.get_ref().peer_certificate() {
        Ok(Some(cert)) => cert.to_der().map_err(|_| failed())?,
        _ => return Err(failed()),
    };
    let fingerprint = fingerprint(&der);
    if reg.trusts(&fingerprint) {
        return Ok(tls);
    }
    let (reply, answer) = oneshot::channel();
    let changed = !reg.pinned.is_empty();
    sink(id, NetEvent::UntrustedCertificate(CertificatePrompt { fingerprint, reason, changed, reply }));
    match answer.await {
        Ok(true) => Ok(tls),
        _ => Err(("The server's certificate wasn't trusted.".into(), false)),
    }
}

/// SHA-256 of a DER certificate as colon-separated upper-case hex, the way most clients
/// and servers show it.
pub fn fingerprint(der: &[u8]) -> String {
    let hex: Vec<String> = Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect();
    hex.join(":")
}

/// Compares fingerprints ignoring case, colons and spaces, so a hand-edited or pasted one
/// still matches. A blank one matches nothing.
pub fn same_fingerprint(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_uppercase()).collect::<String>();
    let a = norm(a);
    !a.is_empty() && a == norm(b)
}

/// Registers and runs the connection over an open stream until it ends.
async fn session<S>(id: ServerId, reg: Registration, stream: S, sink: EventSink) -> (String, bool)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, mut write) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut conn = Connection { id, sink, sender: Sender(tx), nick: reg.nick.clone(), alt_nick: 0, registered: false, closing: Arc::default(), reg };
    let closing = conn.closing.clone();
    conn.register();
    conn.event(NetEvent::Connected(conn.sender.clone()));
    conn.event(NetEvent::NickChanged(conn.nick.clone()));

    // Only ends with an error: `conn` holds a sender, so `rx` stays open.
    let writer = async move {
        while let Some(msg) = rx.recv().await {
            if matches!(msg.command, Command::QUIT(_)) {
                closing.store(true, Ordering::Relaxed);
            }
            write.write_all(wire(&msg).as_bytes()).await?;
            write.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::select! {
        end = conn.read_loop(BufReader::new(read)) => end,
        Err(e) = writer => (format!("Couldn't send: {}", describe(&e)), true),
    }
}

struct Connection {
    id: ServerId,
    sink: EventSink,
    sender: Sender,
    reg: Registration,
    /// Our nick as far as we know.
    nick: String,
    /// How many of `reg.alt_nicks` we've tried.
    alt_nick: usize,
    /// Seen 001.
    registered: bool,
    /// We sent QUIT or the server sent ERROR: the connection is about to close.
    closing: Arc<AtomicBool>,
}

impl Connection {
    fn event(&self, event: NetEvent) {
        (self.sink)(self.id, event);
    }

    /// Queues a message. Can't fail: the receiver lives as long as the session.
    fn send(&self, msg: impl Into<Message>) {
        let _ = self.sender.send(msg);
    }

    fn register(&self) {
        if !self.reg.password.is_empty() {
            self.send(Command::PASS(self.reg.password.clone()));
        }
        self.send(Command::NICK(self.reg.nick.clone()));
        self.send(Command::USER(self.reg.username.clone(), "0".into(), self.reg.realname.clone()));
    }

    /// Reads and handles lines until the connection ends, PINGing the server when it's quiet.
    async fn read_loop(&mut self, mut reader: impl AsyncBufRead + Unpin) -> (String, bool) {
        let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        // Set when we PING; any line from the server clears it.
        let mut deadline: Option<Instant> = None;
        // Kept across loop turns: a read cancelled by the other branches leaves its bytes here.
        let mut line = Vec::new();
        loop {
            let timeout = async {
                match deadline {
                    Some(d) => sleep_until(d).await,
                    None => pending().await,
                }
            };
            let mut limited = (&mut reader).take((MAX_LINE - line.len()) as u64);
            tokio::select! {
                read = limited.read_until(b'\n', &mut line) => {
                    match read {
                        Ok(0) if line.is_empty() => return ("Connection closed.".into(), true),
                        // Cut short by the end of the stream or MAX_LINE: handle what we have.
                        Ok(_) => {}
                        // Some servers reset the connection rather than closing it, and the
                        // reset can even discard the ERROR line before it's read.
                        Err(_) if self.closing.load(Ordering::Relaxed) => return ("Connection closed.".into(), true),
                        Err(e) => return (describe(&e), true),
                    }
                    deadline = None;
                    let end = self.handle_line(&line);
                    line.clear();
                    if let Some(end) = end {
                        return end;
                    }
                }
                _ = ping.tick() => {
                    self.send(Command::PING(chrono::Utc::now().timestamp().to_string(), None));
                    deadline.get_or_insert(Instant::now() + PING_TIMEOUT);
                }
                _ = timeout => {
                    return (format!("No reply from the server in {} seconds.", PING_TIMEOUT.as_secs()), true);
                }
            }
        }
    }

    /// Handles one line from the server. `Some` ends the connection with that reason.
    fn handle_line(&mut self, line: &[u8]) -> Option<(String, bool)> {
        // Invalid UTF-8 (a Latin-1 client, say) shouldn't lose the whole line.
        let text = String::from_utf8_lossy(line);
        if text.trim().is_empty() {
            return None;
        }
        let msg: Message = match text.parse() {
            Ok(msg) => msg,
            Err(e) => {
                self.event(NetEvent::Error(format!("Couldn't parse {:?}: {e}", text.trim_end())));
                return None;
            }
        };
        let source_is_me = msg.source_nickname().is_some_and(|n| irc_eq(n, &self.nick));
        self.event(NetEvent::Message(msg.clone()));

        match &msg.command {
            Command::PING(token, _) => self.send(Command::PONG(token.clone(), None)),
            Command::Response(Response::RPL_WELCOME, args) => {
                self.registered = true;
                if let Some(nick) = args.first() {
                    self.set_nick(nick);
                }
                for line in &self.reg.connect_commands {
                    if let Err(e) = parse_raw(line).map(|msg| self.send(msg)) {
                        self.event(NetEvent::Error(format!("Connect command {line:?}: {e}")));
                    }
                }
            }
            // After registration the user picked the nick; they can pick another.
            Command::Response(Response::ERR_NICKNAMEINUSE | Response::ERR_ERRONEOUSNICKNAME, _) if !self.registered => {
                let Some(alt) = self.reg.alt_nicks.get(self.alt_nick).cloned() else {
                    return Some(("Every nickname is in use.".into(), false));
                };
                self.alt_nick += 1;
                self.send(Command::NICK(alt.clone()));
                self.set_nick(&alt);
            }
            Command::NICK(new) if source_is_me => self.set_nick(new),
            Command::ERROR(_) => self.closing.store(true, Ordering::Relaxed),
            Command::PRIVMSG(_, body) if !source_is_me => {
                if let (Some(from), Some((query, args))) = (msg.source_nickname(), parse_ctcp(body)) {
                    if let Some(reply) = ctcp_reply(query, args) {
                        self.send(Command::NOTICE(from.into(), format!("\x01{reply}\x01")));
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn set_nick(&mut self, nick: &str) {
        if self.nick != nick {
            self.nick = nick.to_string();
            self.event(NetEvent::NickChanged(self.nick.clone()));
        }
    }
}

/// A message as sent: one line, cut at any CR or LF in it so text can't smuggle in a
/// second command.
fn wire(msg: &Message) -> String {
    let text = msg.to_string();
    let end = text.find(['\r', '\n']).unwrap_or(text.len());
    format!("{}\r\n", &text[..end])
}

/// The reply to a CTCP query, or `None` to stay quiet.
fn ctcp_reply(query: &str, args: &str) -> Option<String> {
    match query.to_ascii_uppercase().as_str() {
        "VERSION" => Some(format!("VERSION irctest {}", env!("CARGO_PKG_VERSION"))),
        "PING" => Some(format!("PING {args}")),
        "TIME" => Some(format!("TIME {}", chrono::Local::now().to_rfc2822())),
        "CLIENTINFO" => Some("CLIENTINFO ACTION CLIENTINFO PING TIME VERSION".into()),
        _ => None,
    }
}

/// An error with its causes, which often hold the useful part.
fn describe(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    text
}

/// How long to wait before reconnect attempt `attempt` (0-based): 5 s, doubling, at most 5 minutes.
pub fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs((5u64 << attempt.min(6)).min(300))
}

/// Parses a raw line as typed by the user (`PRIVMSG #chan :hi there`), so it is
/// re-serialised correctly when sent.
fn parse_raw(line: &str) -> Result<Message, String> {
    line.trim().parse().map_err(|e| format!("{e}"))
}

/// Sends a raw line as typed by the user. A blank line sends nothing.
pub fn send_raw(sender: &Sender, line: &str) -> Result<(), String> {
    if line.trim().is_empty() {
        return Ok(());
    }
    sender.send(parse_raw(line)?).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
    use tokio::task::JoinHandle;

    fn server() -> ServerConfig {
        ServerConfig { name: "lan".into(), host: "10.0.0.2".into(), ..ServerConfig::default() }
    }

    #[test]
    fn registration_uses_effective_names() {
        let config = Config { nickname: "tyler".into(), ..Config::default() };
        let reg = Registration::new(&config, &server(), &[]).unwrap();
        assert_eq!(reg.nick, "tyler");
        assert_eq!(reg.alt_nicks, vec!["tyler_", "tyler__", "tyler___"]);
        assert_eq!(reg.username, "tyler");
        assert_eq!(reg.host, "10.0.0.2");
        assert_eq!(reg.port, 6667);
    }

    #[test]
    fn registration_needs_nick_and_host() {
        assert!(Registration::new(&Config::default(), &server(), &[]).is_err());
        let config = Config { nickname: "tyler".into(), ..Config::default() };
        assert!(Registration::new(&config, &ServerConfig::default(), &[]).is_err());
    }

    #[test]
    fn reconnect_backoff() {
        let secs: Vec<u64> = (0..9).map(|n| reconnect_delay(n).as_secs()).collect();
        assert_eq!(secs, vec![5, 10, 20, 40, 80, 160, 300, 300, 300]);
        assert_eq!(reconnect_delay(u32::MAX).as_secs(), 300);
    }

    #[test]
    fn raw_lines_parse() {
        assert_eq!(parse_raw("PRIVMSG #test :hello there").unwrap().to_string(), "PRIVMSG #test :hello there\r\n");
        assert_eq!(parse_raw("  JOIN #test ").unwrap().to_string(), "JOIN #test\r\n");
    }

    #[test]
    fn one_line_on_the_wire() {
        let msg = Message::from(Command::PRIVMSG("#test".into(), "hi\r\nQUIT :gotcha".into()));
        assert_eq!(wire(&msg), "PRIVMSG #test :hi\r\n");
        assert_eq!(wire(&parse_raw("JOIN #test").unwrap()), "JOIN #test\r\n");
    }

    #[test]
    fn quit_without_reason() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = Sender(tx);
        sender.send_quit("").unwrap();
        sender.send_quit("bye").unwrap();
        assert_eq!(rx.try_recv().unwrap().to_string(), "QUIT\r\n");
        assert_eq!(rx.try_recv().unwrap().to_string(), "QUIT bye\r\n");
        drop(rx);
        assert!(sender.send_quit("").is_err());
    }

    fn test_reg() -> Registration {
        Registration {
            host: "test".into(),
            port: 6667,
            tls: false,
            password: String::new(),
            nick: "tyler".into(),
            alt_nicks: vec!["tyler_".into(), "tyler__".into()],
            username: "tspivey".into(),
            realname: "Tyler S".into(),
            connect_commands: Vec::new(),
            pinned: String::new(),
            accepted: Vec::new(),
        }
    }

    /// A sink that queues events for the test to read.
    fn channel_sink() -> (EventSink, mpsc::UnboundedReceiver<NetEvent>) {
        let (tx, events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |_, event| {
            let _ = tx.send(event);
        });
        (sink, events)
    }

    #[test]
    fn registration_trusts_saved_and_accepted_fingerprints() {
        let config = Config { nickname: "tyler".into(), ..Config::default() };
        let server = ServerConfig { fingerprint: " AB:CD ".into(), ..server() };
        let reg = Registration::new(&config, &server, &["12:34".into()]).unwrap();
        assert!(reg.trusts("ab:cd"));
        assert!(reg.trusts("12:34"));
        assert!(!reg.trusts("56:78"));
        assert!(!Registration::new(&config, &self::server(), &[]).unwrap().trusts(""));
    }

    #[test]
    fn fingerprints() {
        // SHA-256 of "abc".
        assert_eq!(
            fingerprint(b"abc"),
            "BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
        );
        assert!(same_fingerprint("ba:78:16", "BA7816"));
        assert!(same_fingerprint("BA 78 16", "BA:78:16"));
        assert!(!same_fingerprint("BA:78:16", "BA:78:17"));
        assert!(!same_fingerprint("", ""));
    }

    /// The server's end of a session running over an in-memory stream.
    struct Fake {
        lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
        write: WriteHalf<DuplexStream>,
        events: mpsc::UnboundedReceiver<NetEvent>,
        session: JoinHandle<(String, bool)>,
    }

    impl Fake {
        fn start(password: &str, connect_commands: &[&str]) -> Self {
            let reg = Registration {
                password: password.into(),
                connect_commands: connect_commands.iter().map(|s| s.to_string()).collect(),
                ..test_reg()
            };
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (sink, events) = channel_sink();
            let session = tokio::spawn(session(1, reg, client, sink));
            let (read, write) = tokio::io::split(server);
            Self { lines: BufReader::new(read).lines(), write, events, session }
        }

        async fn send(&mut self, line: &str) {
            self.write.write_all(format!("{line}\r\n").as_bytes()).await.unwrap();
        }

        /// The next line the client sent.
        async fn recv(&mut self) -> String {
            self.lines.next_line().await.unwrap().expect("client closed the connection")
        }

        async fn event(&mut self) -> NetEvent {
            self.events.recv().await.expect("no more events")
        }

        /// Skips events up to and including the Message for the line just sent.
        async fn skip_message(&mut self) {
            while !matches!(self.event().await, NetEvent::Message(_)) {}
        }

        /// Reads past registration and the startup events.
        async fn registered(password: &str, connect_commands: &[&str]) -> Self {
            let mut fake = Self::start(password, connect_commands);
            while !fake.recv().await.starts_with("USER") {}
            assert!(matches!(fake.event().await, NetEvent::Connected(_)));
            assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "tyler"));
            fake
        }
    }

    #[tokio::test]
    async fn registers() {
        let mut fake = Fake::start("user/net:pass", &[]);
        assert_eq!(fake.recv().await, "PASS user/net:pass");
        assert_eq!(fake.recv().await, "NICK tyler");
        assert_eq!(fake.recv().await, "USER tspivey 0 * :Tyler S");
        assert!(matches!(fake.event().await, NetEvent::Connected(_)));
        assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "tyler"));
    }

    #[tokio::test]
    async fn no_pass_without_password() {
        let mut fake = Fake::start("", &[]);
        assert_eq!(fake.recv().await, "NICK tyler");
    }

    #[tokio::test]
    async fn answers_ping() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("PING :irc.test").await;
        assert_eq!(fake.recv().await, "PONG irc.test");
        assert!(matches!(fake.event().await, NetEvent::Message(m) if matches!(m.command, Command::PING(..))));
    }

    #[tokio::test]
    async fn welcome_sets_nick_and_sends_connect_commands() {
        let mut fake = Fake::registered("", &["JOIN #test", "PRIVMSG NickServ :IDENTIFY x"]).await;
        fake.send(":irc.test 001 tyler| :Welcome").await;
        fake.skip_message().await;
        assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "tyler|"));
        assert_eq!(fake.recv().await, "JOIN #test");
        assert_eq!(fake.recv().await, "PRIVMSG NickServ :IDENTIFY x");
    }

    #[tokio::test]
    async fn bad_connect_command_is_reported() {
        let mut fake = Fake::registered("", &[":a :b"]).await;
        fake.send(":irc.test 001 tyler :Welcome").await;
        fake.skip_message().await;
        assert!(matches!(fake.event().await, NetEvent::Error(e) if e.starts_with("Connect command \":a :b\"")));
    }

    #[tokio::test]
    async fn alt_nicks_then_gives_up() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":irc.test 433 * tyler :Nickname is already in use").await;
        assert_eq!(fake.recv().await, "NICK tyler_");
        fake.skip_message().await;
        assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "tyler_"));
        fake.send(":irc.test 433 * tyler_ :Nickname is already in use").await;
        assert_eq!(fake.recv().await, "NICK tyler__");
        fake.send(":irc.test 433 * tyler__ :Nickname is already in use").await;
        assert_eq!(fake.session.await.unwrap(), ("Every nickname is in use.".into(), false));
    }

    #[tokio::test]
    async fn nick_in_use_after_registration_is_left_to_the_user() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":irc.test 001 tyler :Welcome").await;
        fake.send(":irc.test 433 tyler other :Nickname is already in use").await;
        fake.send("PING :x").await;
        assert_eq!(fake.recv().await, "PONG x");
    }

    #[tokio::test]
    async fn follows_our_nick_changes_only() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":bob!b@host NICK robert").await;
        fake.send(":Tyler!t@host NICK newnick").await;
        fake.skip_message().await;
        fake.skip_message().await;
        assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "newnick"));
    }

    #[tokio::test]
    async fn answers_ctcp() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":bob!b@host PRIVMSG tyler :\x01VERSION\x01").await;
        assert_eq!(fake.recv().await, format!("NOTICE bob :\x01VERSION irctest {}\x01", env!("CARGO_PKG_VERSION")));
        fake.send(":bob!b@host PRIVMSG #test :\x01PING 12345\x01").await;
        assert_eq!(fake.recv().await, "NOTICE bob :\x01PING 12345\x01");
        // Not answered: ACTION, unknown queries, and our own messages.
        fake.send(":bob!b@host PRIVMSG tyler :\x01ACTION waves\x01").await;
        fake.send(":bob!b@host PRIVMSG tyler :\x01FINGER\x01").await;
        fake.send(":tyler!t@host PRIVMSG tyler :\x01VERSION\x01").await;
        fake.send("PING :done").await;
        assert_eq!(fake.recv().await, "PONG done");
    }

    #[tokio::test]
    async fn survives_bad_lines() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("").await;
        fake.send(":irc.test :oops").await;
        assert!(matches!(fake.event().await, NetEvent::Error(e) if e.starts_with("Couldn't parse")));
        fake.write.write_all(b":bob!b@host PRIVMSG #test :caf\xe9\r\n").await.unwrap();
        match fake.event().await {
            NetEvent::Message(m) => assert_eq!(m.command, Command::PRIVMSG("#test".into(), "caf\u{FFFD}".into())),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn overlong_line_is_cut() {
        let mut fake = Fake::registered("", &[]).await;
        let long = format!("PRIVMSG #test :{}", "x".repeat(MAX_LINE));
        fake.send(&long).await;
        fake.send("PING :after").await;
        assert_eq!(fake.recv().await, "PONG after");
    }

    #[tokio::test]
    async fn sends_queued_messages() {
        let mut fake = Fake::start("", &[]);
        fake.recv().await;
        fake.recv().await;
        let NetEvent::Connected(sender) = fake.event().await else { panic!() };
        send_raw(&sender, "JOIN #test").unwrap();
        sender.send_action("#test", "waves").unwrap();
        assert_eq!(fake.recv().await, "JOIN #test");
        assert_eq!(fake.recv().await, "PRIVMSG #test :\x01ACTION waves\x01");
    }

    #[tokio::test]
    async fn server_closing_ends_session() {
        let fake = Fake::registered("", &[]).await;
        drop(fake.write);
        drop(fake.lines);
        assert_eq!(fake.session.await.unwrap(), ("Connection closed.".into(), true));
    }

    #[tokio::test(start_paused = true)]
    async fn pings_and_times_out() {
        let mut fake = Fake::registered("", &[]).await;
        let start = Instant::now();
        assert!(fake.recv().await.starts_with("PING "));
        assert_eq!(start.elapsed(), PING_INTERVAL);
        // Any line counts as a reply.
        fake.send(":irc.test NOTICE tyler :hi").await;
        assert!(fake.recv().await.starts_with("PING "));
        assert_eq!(start.elapsed(), PING_INTERVAL * 2);
        let (reason, retry) = fake.session.await.unwrap();
        assert_eq!(reason, "No reply from the server in 20 seconds.");
        assert!(retry);
        assert_eq!(start.elapsed(), PING_INTERVAL * 2 + PING_TIMEOUT);
    }

    /// A local TLS server with the self-signed certificate in `testdata` (RSA: Windows won't
    /// import other PKCS#8 keys). Each accepted connection that completes the handshake sends
    /// its first line (the client's first command) to `lines`.
    struct TlsServer {
        port: u16,
        fingerprint: String,
        lines: mpsc::UnboundedReceiver<String>,
    }

    impl TlsServer {
        async fn start() -> Self {
            let cert = include_bytes!("../testdata/selfsigned.crt");
            let key = include_bytes!("../testdata/selfsigned.key");
            let fingerprint = fingerprint(&native_tls::Certificate::from_pem(cert).unwrap().to_der().unwrap());
            let identity = native_tls::Identity::from_pkcs8(cert, key).unwrap();
            let acceptor = tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).unwrap());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, lines) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let (acceptor, tx) = (acceptor.clone(), tx.clone());
                    tokio::spawn(async move {
                        // The verifying client hangs up mid-handshake.
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        let mut line = String::new();
                        if BufReader::new(tls).read_line(&mut line).await.is_ok() {
                            let _ = tx.send(line.trim_end().to_string());
                        }
                    });
                }
            });
            Self { port, fingerprint, lines }
        }

        /// Runs a connection to this server; returns its events and the result.
        fn connect(&self, pinned: &str, accepted: &[&str]) -> (mpsc::UnboundedReceiver<NetEvent>, JoinHandle<(String, bool)>) {
            let reg = Registration {
                host: "127.0.0.1".into(),
                port: self.port,
                tls: true,
                pinned: pinned.into(),
                accepted: accepted.iter().map(|s| s.to_string()).collect(),
                ..test_reg()
            };
            let (sink, events) = channel_sink();
            (events, tokio::spawn(run(1, reg, sink)))
        }
    }

    async fn expect_prompt(events: &mut mpsc::UnboundedReceiver<NetEvent>) -> CertificatePrompt {
        match events.recv().await {
            Some(NetEvent::UntrustedCertificate(prompt)) => prompt,
            other => panic!("expected a certificate prompt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn untrusted_certificate_asks_then_carries_on() {
        let mut server = TlsServer::start().await;
        let (mut events, _run) = server.connect("", &[]);
        let prompt = expect_prompt(&mut events).await;
        // As `openssl x509 -noout -fingerprint -sha256` shows it.
        assert_eq!(
            prompt.fingerprint,
            "F9:87:93:75:41:F5:FB:73:8E:85:06:EE:99:2B:D2:E8:A6:B3:22:78:73:61:A6:6C:53:9B:4A:E3:CC:5E:05:5D"
        );
        assert!(!prompt.changed);
        assert!(!prompt.reason.is_empty());
        prompt.answer(true);
        assert!(matches!(events.recv().await, Some(NetEvent::Connected(_))));
        assert_eq!(server.lines.recv().await.unwrap(), "NICK tyler");
    }

    #[tokio::test]
    async fn refused_certificate_disconnects_for_good() {
        let server = TlsServer::start().await;
        let (mut events, run) = server.connect("", &[]);
        expect_prompt(&mut events).await.answer(false);
        assert_eq!(run.await.unwrap(), ("The server's certificate wasn't trusted.".into(), false));
    }

    #[tokio::test]
    async fn trusted_fingerprints_skip_the_prompt() {
        let mut server = TlsServer::start().await;
        let fingerprint = server.fingerprint.to_lowercase();
        for (pinned, accepted) in [(fingerprint.as_str(), vec![]), ("", vec![fingerprint.as_str()])] {
            let (mut events, _run) = server.connect(pinned, &accepted);
            assert!(matches!(events.recv().await, Some(NetEvent::Connected(_))));
            assert_eq!(server.lines.recv().await.unwrap(), "NICK tyler");
        }
    }

    #[tokio::test]
    async fn changed_certificate_says_so() {
        let server = TlsServer::start().await;
        let (mut events, _run) = server.connect("00:11:22", &[]);
        let prompt = expect_prompt(&mut events).await;
        assert!(prompt.changed);
        assert_eq!(prompt.fingerprint, server.fingerprint);
    }

    #[tokio::test]
    async fn non_tls_server_reports_the_handshake_error() {
        // Accepts and closes, like a plain-text port would after garbage.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let _ = tcp.write_all(b"ERROR :not TLS\r\n").await;
            }
        });
        let reg = Registration { host: "127.0.0.1".into(), port, tls: true, ..test_reg() };
        let (sink, mut events) = channel_sink();
        let (reason, retry) = run(1, reg, sink).await;
        assert!(reason.starts_with("TLS handshake failed: "), "{reason}");
        assert!(retry);
        assert!(events.try_recv().is_err());
    }
}
