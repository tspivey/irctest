//! One tokio task per server connection: connects (with TLS if asked), registers (logging in
//! with SASL PLAIN if asked), answers PINGs and CTCP queries, and passes every message on.
//!
//! The task reports what happens through an event sink; the caller sends with the
//! [`Sender`] handed over in [`NetEvent::Connected`].

use std::fmt;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use irc_proto::{CapSubCommand, Command, Message, Response};
use sha2::{Digest, Sha256};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{Instant, interval_at, sleep_until};

use crate::bouncer::{ConnectionRole, NetworkUpdate, Networks};
use crate::cap::{CapAction, CapOptions, Capabilities, Negotiation};
use crate::config::{Config, ServerConfig};
use crate::format::parse_ctcp;
use crate::playback::{
    BatchBoundary, BatchEvent, BatchTracker, HistoryAction, HistoryQueue, HistoryReason,
    HistoryRequest, HistorySupport,
};
use crate::route::irc_eq;

/// Identifies a connection; chosen by the caller.
pub type ServerId = u32;

#[derive(Debug)]
pub enum NetEvent {
    /// TCP (and TLS) is up and registration has been sent. Use the sender for everything else.
    Connected(Sender),
    Message(Message),
    Capabilities(Capabilities),
    Role(ConnectionRole),
    Network(NetworkUpdate),
    NetworkSnapshot(Vec<crate::bouncer::Network>),
    Batch(BatchBoundary),
    HistoryMessage {
        batch: String,
        target: Option<String>,
        message: Message,
    },
    HistoryComplete {
        target: String,
        reason: HistoryReason,
        messages: Vec<Message>,
        exhausted: bool,
        pending: bool,
    },
    HistoryTarget {
        target: String,
        latest: String,
    },
    HistoryNotice {
        target: Option<String>,
        text: String,
    },
    ReadMarker(crate::read_marker::Update),
    ReadMarkerFailed {
        target: Option<String>,
        text: String,
    },
    /// Our nick changed (alternative nick on registration, the server's 001, or a NICK of ours).
    /// Sent after the message that caused it.
    NickChanged(String),
    /// Something went wrong but the connection is still up (e.g. an unparseable line).
    Error(String),
    /// An actionable connection status that is not a failed connection.
    Notice(String),
    /// The server's TLS certificate isn't trusted by Windows or by fingerprint. The connection
    /// waits, without registering, until the prompt is answered.
    UntrustedCertificate(CertificatePrompt),
    /// The connection is gone, or never came up. Always the last event.
    /// `retry` is false when reconnecting wouldn't help (no nick set, every nick in use,
    /// certificate refused).
    Disconnected {
        reason: String,
        retry: bool,
    },
}

#[derive(Debug)]
pub struct ConnectionEvent {
    pub attempt: u64,
    pub event: NetEvent,
}
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    pub attempt: u64,
    pub bind_network: Option<String>,
}
pub type EventSink = Arc<dyn Fn(ServerId, ConnectionEvent) + Send + Sync>;

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
pub struct Sender(
    mpsc::UnboundedSender<Message>,
    mpsc::UnboundedSender<HistoryControl>,
);
#[derive(Debug)]
enum HistoryControl {
    Fetch(HistoryRequest),
    Targets(Option<crate::sync_state::Cursor>),
}

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
    pub fn history(&self, request: HistoryRequest) -> Result<(), Closed> {
        self.1
            .send(HistoryControl::Fetch(request))
            .map_err(|_| Closed)
    }
    pub fn discover_history_targets(
        &self,
        since: Option<crate::sync_state::Cursor>,
    ) -> Result<(), Closed> {
        self.1
            .send(HistoryControl::Targets(since))
            .map_err(|_| Closed)
    }
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
/// Longer physical lines are discarded through their terminating newline.
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
    /// Account for SASL PLAIN.
    pub sasl_username: String,
    /// Blank = don't use SASL.
    pub sasl_password: String,
    /// The fingerprint saved for this server; blank if none.
    pub pinned: String,
    /// Fingerprints the user accepted for this run of the program only.
    pub accepted: Vec<String>,
    pub discover_networks: bool,
    pub options: SessionOptions,
}

impl Registration {
    /// `accepted` are certificate fingerprints trusted for now, on top of the saved one.
    /// Errors are user-facing messages.
    pub fn new(
        config: &Config,
        server: &ServerConfig,
        accepted: &[String],
    ) -> Result<Self, String> {
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
            alt_nicks: (1..=3)
                .map(|n| format!("{nick}{}", "_".repeat(n)))
                .collect(),
            username: config.effective_username(server).to_string(),
            realname: config.effective_realname(server).to_string(),
            connect_commands: server.connect_commands.clone(),
            sasl_username: match server.sasl_username.trim() {
                "" => nick.to_string(),
                user => user.to_string(),
            },
            sasl_password: server.sasl_password.clone(),
            pinned: server.fingerprint.trim().to_string(),
            accepted: accepted.to_vec(),
            discover_networks: server.discover_networks,
            options: SessionOptions::default(),
        })
    }

    fn trusts(&self, fingerprint: &str) -> bool {
        std::iter::once(&self.pinned)
            .chain(&self.accepted)
            .any(|f| same_fingerprint(f, fingerprint))
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
    options: SessionOptions,
    sink: EventSink,
) -> AbortHandle {
    let attempt = options.attempt;
    let registration = Registration::new(config, server, accepted).map(|mut r| {
        r.options = options;
        r
    });
    handle
        .spawn(async move {
            let (reason, retry) = match registration {
                Ok(reg) => run(id, reg, sink.clone()).await,
                Err(e) => (e, false),
            };
            sink(
                id,
                ConnectionEvent {
                    attempt,
                    event: NetEvent::Disconnected { reason, retry },
                },
            );
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
    TcpStream::connect((reg.host.as_str(), reg.port))
        .await
        .map_err(|e| (format!("Couldn't connect: {}", describe(&e)), true))
}

type TlsStream = tokio_native_tls::TlsStream<TcpStream>;

/// A TLS handshake; with `verify` false any certificate is accepted.
async fn tls_connect(
    host: &str,
    tcp: TcpStream,
    verify: bool,
) -> Result<TlsStream, native_tls::Error> {
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(!verify)
        .danger_accept_invalid_hostnames(!verify)
        .build()?;
    tokio_native_tls::TlsConnector::from(connector)
        .connect(host, tcp)
        .await
}

/// The handshake failed with `reason`, perhaps over the certificate. Connects again without
/// checking it, and uses that connection if its certificate's fingerprint is trusted or the
/// user says to go ahead. Checking the connection we then use (not an earlier one) means a
/// server can't show one certificate to the check and another to the session.
async fn check_certificate(
    id: ServerId,
    reg: &Registration,
    sink: &EventSink,
    reason: String,
) -> Result<TlsStream, (String, bool)> {
    let failed = || (format!("TLS handshake failed: {reason}"), true);
    // Any failure here means the problem wasn't (only) the certificate: report the first error.
    let tcp = tcp_connect(reg).await?;
    let tls = tls_connect(&reg.host, tcp, false)
        .await
        .map_err(|_| failed())?;
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
    sink(
        id,
        ConnectionEvent {
            attempt: reg.options.attempt,
            event: NetEvent::UntrustedCertificate(CertificatePrompt {
                fingerprint,
                reason,
                changed,
                reply,
            }),
        },
    );
    match answer.await {
        Ok(true) => Ok(tls),
        _ => Err(("The server's certificate wasn't trusted.".into(), false)),
    }
}

/// SHA-256 of a DER certificate as colon-separated upper-case hex, the way most clients
/// and servers show it.
pub fn fingerprint(der: &[u8]) -> String {
    let hex: Vec<String> = Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    hex.join(":")
}

/// Compares fingerprints ignoring case, colons and spaces, so a hand-edited or pasted one
/// still matches. A blank one matches nothing.
pub fn same_fingerprint(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(char::is_ascii_hexdigit)
            .map(|c| c.to_ascii_uppercase())
            .collect::<String>()
    };
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
    let (history_tx, history_rx) = mpsc::unbounded_channel();
    let mut conn = Connection {
        history: HistoryQueue::new(HistorySupport::default()),
        id,
        sink,
        sender: Sender(tx, history_tx),
        nick: reg.nick.clone(),
        alt_nick: 0,
        registered: false,
        sasl: Sasl::Idle,
        batches: BatchTracker::default(),
        networks: Networks::default(),
        role: None,
        bound_id: None,
        role_deadline: None,
        authenticated: false,
        commands_sent: false,
        snapshot_complete: false,
        snapshot_batch: None,
        bind_sent: false,
        failure: None,
        negotiation: Negotiation::new(CapOptions {
            sasl_plain: !reg.sasl_password.is_empty(),
            discover_networks: reg.discover_networks,
            bind_network: reg.options.bind_network.is_some(),
        }),
        negotiation_deadline: Some(Instant::now() + Duration::from_secs(30)),
        closing: Arc::default(),
        reg,
    };
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
        end = conn.read_loop(BufReader::new(read), history_rx) => end,
        Err(e) = writer => (format!("Couldn't send: {}", describe(&e)), true),
    }
}

struct Connection {
    history: HistoryQueue,
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
    sasl: Sasl,
    batches: BatchTracker,
    networks: Networks,
    role: Option<ConnectionRole>,
    bound_id: Option<String>,
    role_deadline: Option<Instant>,
    authenticated: bool,
    commands_sent: bool,
    snapshot_complete: bool,
    snapshot_batch: Option<String>,
    bind_sent: bool,
    failure: Option<(String, bool)>,
    negotiation: Negotiation,
    negotiation_deadline: Option<Instant>,
    /// We sent QUIT or the server sent ERROR: the connection is about to close.
    closing: Arc<AtomicBool>,
}

/// How far SASL login has got during registration.
#[derive(Debug, PartialEq)]
enum Sasl {
    /// Not using SASL, or done with it.
    Idle,
    /// Sent AUTHENTICATE PLAIN.
    Authenticating,
    /// The PLAIN response has been sent; duplicate challenges cannot replay it.
    CredentialsSent,
}

impl Connection {
    fn event(&self, event: NetEvent) {
        (self.sink)(
            self.id,
            ConnectionEvent {
                attempt: self.reg.options.attempt,
                event,
            },
        );
    }

    /// Queues a message. Can't fail: the receiver lives as long as the session.
    fn send(&self, msg: impl Into<Message>) {
        let _ = self.sender.send(msg);
    }

    fn register(&mut self) {
        let actions = self.negotiation.start();
        self.cap_actions(actions);
        if !self.reg.password.is_empty() {
            self.send(Command::PASS(self.reg.password.clone()));
        }
        self.send(Command::NICK(self.reg.nick.clone()));
        self.send(Command::USER(
            self.reg.username.clone(),
            "0".into(),
            self.reg.realname.clone(),
        ));
    }

    /// Reads and handles lines until the connection ends, PINGing the server when it's quiet.
    async fn read_loop(
        &mut self,
        mut reader: impl AsyncBufRead + Unpin,
        mut history_rx: mpsc::UnboundedReceiver<HistoryControl>,
    ) -> (String, bool) {
        let mut history_tick = interval_at(
            Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        // Set when we PING; any line from the server clears it.
        let mut deadline: Option<Instant> = None;
        // Kept across loop turns: a read cancelled by the other branches leaves its bytes here.
        let mut line = Vec::new();
        let mut discarding = false;
        loop {
            let timeout = async {
                match deadline {
                    Some(d) => sleep_until(d).await,
                    None => pending().await,
                }
            };
            let negotiation_timeout = async {
                match self.negotiation_deadline {
                    Some(d) => sleep_until(d).await,
                    None => pending().await,
                }
            };
            let role_timeout = async {
                match self.role_deadline {
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
                    let complete = line.ends_with(b"\n");
                    if discarding {
                        if complete { discarding = false; }
                        line.clear();
                        continue;
                    }
                    if line.len() == MAX_LINE && !complete {
                        discarding = true;
                        line.clear();
                        self.event(NetEvent::Error("An oversized IRC line was discarded.".into()));
                        continue;
                    }
                    let end = self.handle_line(&line);
                    line.clear();
                    if let Some(end) = end {
                        return end;
                    }
                }
                Some(control) = history_rx.recv() => {
                    let target = match &control { HistoryControl::Fetch(r) => Some(r.target.clone()), _ => None };
                    if !self.negotiation.enabled().enabled("draft/chathistory") {
                        self.event(NetEvent::HistoryNotice { target, text: "This server does not support chat history.".into() });
                    } else {
                        let result = match control { HistoryControl::Fetch(r) => self.history.request(r, Instant::now()), HistoryControl::Targets(since) => self.history.targets(since, Instant::now()) };
                        match result { Ok(actions) => self.history_actions(actions), Err(text) => self.event(NetEvent::HistoryNotice {target,text}) }
                    }
                }
                _ = history_tick.tick() => { let actions = self.history.tick(Instant::now()); self.history_actions(actions); }
                _ = role_timeout => self.decide_role(),
                _ = negotiation_timeout => return ("Capability negotiation or authentication timed out after 30 seconds.".into(), true),
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
        let _validated: Message = match text.parse() {
            Ok(msg) => msg,
            Err(_) => {
                self.event(NetEvent::Error("Couldn't parse an IRC message.".into()));
                return None;
            }
        };
        let batch_event = match self.batches.ingest_line(&text) {
            Ok(e) => e,
            Err(error) => return Some((format!("Batch protocol error: {error}"), true)),
        };
        let requested = match &batch_event {
            BatchEvent::Message { batch: Some(b), .. } => self.history.handles(b),
            _ => false,
        };
        if let BatchEvent::Message {
            message,
            history: true,
            ..
        } = &batch_event
        {
            let targets = matches!(&message.command, Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("CHATHISTORY") && args.first().is_some_and(|a| a.eq_ignore_ascii_case("TARGETS")));
            if !matches!(message.command, Command::PRIVMSG(..) | Command::NOTICE(..)) && !targets {
                return None;
            }
        }
        match self.history.handle(&batch_event, Instant::now()) {
            Ok(actions) => self.history_actions(actions),
            Err(e) => return Some((e, true)),
        }
        let msg = match Ok::<_, String>(batch_event) {
            Ok(BatchEvent::Boundary(b)) => {
                match &b {
                    BatchBoundary::Start(m)
                        if m.kind == "soju.im/bouncer-networks"
                            && !m.history
                            && m.parent.is_none()
                            && self.can_collect_networks() =>
                    {
                        if self.snapshot_batch.is_some() {
                            return Some(("Overlapping bouncer network snapshots.".into(), true));
                        }
                        self.snapshot_batch = Some(m.id.clone());
                        self.networks.begin_snapshot();
                    }
                    BatchBoundary::End(m)
                        if self.snapshot_batch.as_deref() == Some(m.id.as_str()) =>
                    {
                        self.snapshot_batch = None;
                        self.snapshot_complete = true;
                        let removed = self.networks.finish_snapshot();
                        if self.role == Some(ConnectionRole::Control) {
                            for u in removed {
                                self.event(NetEvent::Network(u));
                            }
                            self.event(NetEvent::NetworkSnapshot(
                                self.networks.all().cloned().collect(),
                            ));
                        }
                    }
                    _ => {}
                }
                self.event(NetEvent::Batch(b));
                return None;
            }
            Ok(BatchEvent::Message {
                message,
                history: true,
                batch,
                target,
            }) => {
                if requested {
                    return None;
                }
                self.event(NetEvent::HistoryMessage {
                    batch: batch.unwrap_or_default(),
                    target,
                    message,
                });
                return None;
            }
            Ok(BatchEvent::Message { message, .. }) => message,
            Err(error) => return Some((format!("Batch protocol error: {error}"), true)),
        };
        if let Command::Raw(cmd, args) = &msg.command {
            if ["FAIL", "WARN", "NOTE"]
                .iter()
                .any(|c| cmd.eq_ignore_ascii_case(c))
                && args
                    .first()
                    .is_some_and(|a| a.eq_ignore_ascii_case("CHATHISTORY"))
            {
                return None;
            }
            if cmd.eq_ignore_ascii_case("BOUNCER") {
                if !self.can_collect_networks() || self.bound_id.is_some() {
                    return None;
                }
                match self.networks.apply(&msg) {
                    Ok(Some(update)) if self.role == Some(ConnectionRole::Control) => {
                        self.event(NetEvent::Network(update))
                    }
                    Err(e) => return Some((e, true)),
                    _ => {}
                }
                return None;
            }
            if cmd.eq_ignore_ascii_case("FAIL")
                && args
                    .first()
                    .is_some_and(|a| a.eq_ignore_ascii_case("BOUNCER"))
                && self.reg.options.bind_network.is_some()
            {
                return Some(("Bouncer binding failed.".into(), false));
            }
            if cmd.eq_ignore_ascii_case("FAIL")
                && args
                    .first()
                    .is_some_and(|a| a.eq_ignore_ascii_case("MARKREAD"))
            {
                let target = args.get(2).filter(|s| crate::bouncer::valid_id(s)).cloned();
                self.event(NetEvent::ReadMarkerFailed {
                    target,
                    text: "The server rejected the read marker.".into(),
                });
                return None;
            }
        }
        match crate::read_marker::parse(&msg) {
            Ok(Some(update)) => {
                self.event(NetEvent::ReadMarker(update));
                return None;
            }
            Err(error) => {
                self.event(NetEvent::HistoryNotice {
                    target: None,
                    text: error,
                });
                return None;
            }
            _ => {}
        }
        // A rejected child must never publish a welcome or create unbound GUI state.
        if matches!(msg.command, Command::Response(Response::RPL_WELCOME, _))
            && self.reg.options.bind_network.is_some()
            && (!self.bind_sent || !self.authenticated || self.negotiation.unfinished())
        {
            return Some((
                "The bouncer completed registration before authenticated binding.".into(),
                false,
            ));
        }
        let source_is_me = msg.source_nickname().is_some_and(|n| irc_eq(n, &self.nick));
        if let Some(safe) = safe_protocol_message(&msg) {
            self.event(NetEvent::Message(safe));
        }

        match &msg.command {
            Command::PING(token, _) => self.send(Command::PONG(token.clone(), None)),
            Command::Response(Response::RPL_WELCOME, args) => {
                if self.reg.options.bind_network.is_some()
                    && (self.negotiation.unfinished() || !self.authenticated)
                {
                    return Some((
                        "The bouncer completed registration before authenticated binding.".into(),
                        false,
                    ));
                }
                self.registered = true;
                self.role_deadline = Some(Instant::now() + Duration::from_secs(5));
                // The server didn't wait for SASL (or doesn't know CAP).
                self.sasl = Sasl::Idle;
                self.negotiation.abandon_registration();
                self.negotiation_deadline = None;
                if let Some(nick) = args.first() {
                    self.set_nick(nick);
                }
                if !self.discovery_possible() {
                    self.run_connect_commands();
                }
            }
            Command::Response(Response::RPL_ISUPPORT, args) => {
                for arg in args {
                    if let Some(id) = arg.strip_prefix("BOUNCER_NETID=") {
                        if !id.is_empty() {
                            if !crate::bouncer::valid_id(id) {
                                return Some(("Invalid bouncer network identifier.".into(), false));
                            }
                            if self
                                .reg
                                .options
                                .bind_network
                                .as_ref()
                                .is_some_and(|wanted| wanted != id)
                            {
                                return Some((
                                    "The bouncer bound this session to a different network.".into(),
                                    false,
                                ));
                            }
                            self.bound_id = Some(id.into());
                        }
                    }
                    if let Some(n) = arg
                        .strip_prefix("CHATHISTORY=")
                        .and_then(|n| n.parse::<usize>().ok())
                    {
                        self.history.support.limit = (n > 0).then_some(n);
                    }
                    if let Some(refs) = arg.strip_prefix("MSGREFTYPES=") {
                        self.history.support.timestamp_refs =
                            refs.split(',').any(|r| r == "timestamp");
                        self.history.support.msgid_refs = refs.split(',').any(|r| r == "msgid");
                    }
                }
            }
            Command::Response(Response::RPL_ENDOFMOTD | Response::ERR_NOMOTD, _) => {
                self.decide_role()
            }
            // After registration the user picked the nick; they can pick another.
            Command::Response(Response::ERR_NICKNAMEINUSE | Response::ERR_ERRONEOUSNICKNAME, _)
                if !self.registered =>
            {
                let Some(alt) = self.reg.alt_nicks.get(self.alt_nick).cloned() else {
                    return Some(("Every nickname is in use.".into(), false));
                };
                self.alt_nick += 1;
                self.send(Command::NICK(alt.clone()));
                self.set_nick(&alt);
            }
            Command::CAP(..) => self.handle_cap(&msg.command),
            Command::Response(Response::ERR_UNKNOWNCOMMAND, args)
                if args.iter().any(|a| a.eq_ignore_ascii_case("CAP")) =>
            {
                if self.reg.options.bind_network.is_some() {
                    return Some((
                        "Authenticated bouncer binding requires capability negotiation.".into(),
                        false,
                    ));
                }
                self.negotiation.abandon_registration();
                self.negotiation_deadline = None;
                self.sasl = Sasl::Idle;
            }
            Command::AUTHENTICATE(data) if data == "+" && self.sasl == Sasl::Authenticating => {
                self.send_credentials()
            }
            Command::Response(Response::RPL_SASLSUCCESS | Response::ERR_SASLALREADY, _)
                if self.sasl != Sasl::Idle =>
            {
                self.authenticated = true;
                self.end_sasl(None)
            }
            Command::Response(
                Response::ERR_NICKLOCKED
                | Response::ERR_SASLFAIL
                | Response::ERR_SASLTOOLONG
                | Response::ERR_SASLABORT,
                _,
            ) if self.sasl != Sasl::Idle => {
                self.end_sasl(Some("SASL login failed; carrying on without logging in."))
            }
            Command::NICK(new) if source_is_me => self.set_nick(new),
            Command::ERROR(_) => self.closing.store(true, Ordering::Relaxed),
            Command::PRIVMSG(_, body) if !source_is_me => {
                if let (Some(from), Some((query, args))) = (msg.source_nickname(), parse_ctcp(body))
                {
                    if let Some(reply) = ctcp_reply(query, args) {
                        self.send(Command::NOTICE(from.into(), format!("\x01{reply}\x01")));
                    }
                }
            }
            _ => {}
        }
        self.failure.take()
    }

    fn handle_cap(&mut self, cmd: &Command) {
        let actions = self.negotiation.handle_cap(cmd);
        self.cap_actions(actions);
    }

    fn cap_actions(&mut self, actions: Vec<CapAction>) {
        for action in actions {
            match action {
                CapAction::Send(m) => self.send(m),
                CapAction::AuthenticatePlain => {
                    self.sasl = Sasl::Authenticating;
                    self.send(Command::AUTHENTICATE("PLAIN".into()));
                }
                CapAction::Changed(caps) => {
                    if !caps.enabled("soju.im/bouncer-networks") {
                        self.snapshot_batch = None;
                        self.snapshot_complete = false;
                        self.networks.cancel_snapshot();
                    }
                    if !caps.enabled("draft/chathistory") {
                        self.history.cancel();
                    }
                    self.event(NetEvent::Capabilities(caps));
                }
                CapAction::Note(note) => self.event(NetEvent::Error(note)),
                CapAction::Ready => self.finish_negotiation(),
            }
        }
    }

    /// The single registration barrier: authentication and BIND precede CAP END.
    fn finish_negotiation(&mut self) {
        if let Some(id) = &self.reg.options.bind_network {
            if !crate::bouncer::valid_id(id)
                || !self.authenticated
                || !self
                    .negotiation
                    .enabled()
                    .enabled("soju.im/bouncer-networks")
            {
                self.failure = Some((
                    "Authenticated bouncer binding is unavailable.".into(),
                    false,
                ));
                return;
            }
            self.send(Command::Raw(
                "BOUNCER".into(),
                vec!["BIND".into(), id.clone()],
            ));
            self.bind_sent = true;
        }
        self.send(Command::CAP(None, CapSubCommand::END, None, None));
        self.negotiation_deadline = None;
    }

    /// Answers the server's `AUTHENTICATE +` with the PLAIN credentials (no authorization
    /// identity), in 400-byte pieces. A last piece of exactly 400 is followed by `+`.
    fn send_credentials(&mut self) {
        self.sasl = Sasl::CredentialsSent;
        let payload = BASE64.encode(format!(
            "\0{}\0{}",
            self.reg.sasl_username, self.reg.sasl_password
        ));
        for chunk in payload.as_bytes().chunks(400) {
            self.send(Command::AUTHENTICATE(
                String::from_utf8_lossy(chunk).into_owned(),
            ));
        }
        if payload.len() % 400 == 0 {
            self.send(Command::AUTHENTICATE("+".into()));
        }
    }

    /// Finishes capability negotiation, so registration completes. `note` says why we're
    /// carrying on without logging in.
    fn end_sasl(&mut self, note: Option<&str>) {
        if let Some(note) = note {
            if self.reg.options.bind_network.is_some() {
                self.failure = Some((
                    "Bouncer authentication failed; binding cancelled.".into(),
                    false,
                ));
                return;
            }
            self.event(NetEvent::Error(note.into()));
        }
        self.sasl = Sasl::Idle;
        let actions = self.negotiation.finish_sasl();
        self.cap_actions(actions);
    }

    fn history_actions(&mut self, actions: Vec<HistoryAction>) {
        for action in actions {
            match action {
                HistoryAction::Send(m) => self.send(m),
                HistoryAction::Target { target, latest } => {
                    self.event(NetEvent::HistoryTarget { target, latest })
                }
                HistoryAction::Complete {
                    target,
                    reason,
                    messages,
                    exhausted,
                    pending,
                } => self.event(NetEvent::HistoryComplete {
                    target,
                    reason,
                    messages,
                    exhausted,
                    pending,
                }),
                HistoryAction::Notice { target, text } => {
                    self.event(NetEvent::HistoryNotice { target, text })
                }
            }
        }
    }

    fn can_collect_networks(&self) -> bool {
        self.reg.discover_networks
            && self.reg.options.bind_network.is_none()
            && !self.reg.sasl_password.is_empty()
            && self
                .negotiation
                .enabled()
                .enabled("soju.im/bouncer-networks")
    }
    fn discovery_possible(&self) -> bool {
        self.can_collect_networks() && self.authenticated
    }
    fn decide_role(&mut self) {
        if !self.registered || self.role.is_some() {
            return;
        }
        self.role_deadline = None;
        let role = if let Some(id) = self
            .reg
            .options
            .bind_network
            .as_ref()
            .or(self.bound_id.as_ref())
        {
            ConnectionRole::Bound(id.clone())
        } else if self.discovery_possible() {
            ConnectionRole::Control
        } else {
            ConnectionRole::Direct
        };
        self.role = Some(role.clone());
        self.event(NetEvent::Role(role.clone()));
        if role == ConnectionRole::Direct
            && self.reg.discover_networks
            && self.reg.sasl_password.is_empty()
            && self.negotiation.offered("soju.im/bouncer-networks")
        {
            self.event(NetEvent::Notice("Connected to the bouncer account, but no network sessions were opened. Automatic network connections require a SASL username and password. Set them in Server, Manage servers, Edit, then reconnect.".into()));
        }
        if role == ConnectionRole::Control {
            if self.snapshot_complete {
                self.event(NetEvent::NetworkSnapshot(
                    self.networks.all().cloned().collect(),
                ));
            }
            for n in self.networks.all() {
                self.event(NetEvent::Network(NetworkUpdate::Upsert(n.clone())));
            }
            if !self
                .negotiation
                .enabled()
                .enabled("soju.im/bouncer-networks-notify")
            {
                self.send(Command::Raw("BOUNCER".into(), vec!["LISTNETWORKS".into()]));
            }
        } else {
            self.run_connect_commands();
        }
    }

    fn run_connect_commands(&mut self) {
        if self.commands_sent {
            return;
        }
        self.commands_sent = true;
        for line in &self.reg.connect_commands {
            if let Ok(msg) = parse_raw(line) {
                self.send(msg);
            } else {
                self.event(NetEvent::Error("Invalid connect command.".into()));
            }
        }
    }

    fn set_nick(&mut self, nick: &str) {
        if self.nick != nick {
            self.nick = nick.to_string();
            self.event(NetEvent::NickChanged(self.nick.clone()));
        }
    }
}

/// Strip protocol credentials before any UI, log, or headless event can see them.
fn safe_protocol_message(message: &Message) -> Option<Message> {
    let mut safe = message.clone();
    match &mut safe.command {
        Command::AUTHENTICATE(_) | Command::PASS(_) => return None,
        Command::Raw(cmd, _)
            if ["AUTHENTICATE", "PASS"]
                .iter()
                .any(|c| cmd.eq_ignore_ascii_case(c)) =>
        {
            return None;
        }
        Command::Raw(cmd, args)
            if ["FAIL", "WARN", "NOTE"]
                .iter()
                .any(|c| cmd.eq_ignore_ascii_case(c))
                && args.first().is_some_and(|a| {
                    ["BOUNCER", "AUTHENTICATE", "PASS"]
                        .iter()
                        .any(|c| a.eq_ignore_ascii_case(c))
                }) =>
        {
            let subject = args[0].to_ascii_uppercase();
            *args = vec![
                subject,
                "Operation failed; sensitive details omitted.".into(),
            ];
        }
        Command::Response(code, args) => {
            if args.iter().any(|a| {
                ["BOUNCER", "AUTHENTICATE", "PASS"]
                    .iter()
                    .any(|c| a.eq_ignore_ascii_case(c))
            }) {
                *args = vec![
                    args.first().cloned().unwrap_or_default(),
                    "Protocol operation failed; sensitive details omitted.".into(),
                ];
            } else if matches!(
                code,
                Response::RPL_LOGGEDIN
                    | Response::RPL_LOGGEDOUT
                    | Response::RPL_SASLSUCCESS
                    | Response::ERR_SASLALREADY
                    | Response::RPL_SASLMECHS
            ) {
                *args = vec![
                    args.first().cloned().unwrap_or_default(),
                    "SASL account status updated.".into(),
                ];
            } else if matches!(
                code,
                Response::ERR_NICKLOCKED
                    | Response::ERR_SASLFAIL
                    | Response::ERR_SASLTOOLONG
                    | Response::ERR_SASLABORT
            ) {
                *args = vec![
                    args.first().cloned().unwrap_or_default(),
                    "SASL authentication failed.".into(),
                ];
            }
        }
        _ => {}
    }
    Some(safe)
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
        "PING" if args.is_empty() => Some("PING".into()),
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
    line.trim()
        .parse()
        .map_err(|_| "Invalid IRC command.".into())
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
        ServerConfig {
            name: "lan".into(),
            host: "10.0.0.2".into(),
            ..ServerConfig::default()
        }
    }

    #[test]
    fn registration_uses_effective_names() {
        let config = Config {
            nickname: "tyler".into(),
            ..Config::default()
        };
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
        let config = Config {
            nickname: "tyler".into(),
            ..Config::default()
        };
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
        assert_eq!(
            parse_raw("PRIVMSG #test :hello there").unwrap().to_string(),
            "PRIVMSG #test :hello there\r\n"
        );
        assert_eq!(
            parse_raw("  JOIN #test ").unwrap().to_string(),
            "JOIN #test\r\n"
        );
    }

    #[test]
    fn one_line_on_the_wire() {
        let msg = Message::from(Command::PRIVMSG(
            "#test".into(),
            "hi\r\nQUIT :gotcha".into(),
        ));
        assert_eq!(wire(&msg), "PRIVMSG #test :hi\r\n");
        assert_eq!(wire(&parse_raw("JOIN #test").unwrap()), "JOIN #test\r\n");
    }

    #[test]
    fn quit_without_reason() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (history_tx, _) = mpsc::unbounded_channel();
        let sender = Sender(tx, history_tx);
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
            sasl_username: String::new(),
            sasl_password: String::new(),
            pinned: String::new(),
            accepted: Vec::new(),
            discover_networks: false,
            options: SessionOptions::default(),
        }
    }

    /// A sink that queues events for the test to read.
    fn channel_sink() -> (EventSink, mpsc::UnboundedReceiver<NetEvent>) {
        let (tx, events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |_, event| {
            let _ = tx.send(event.event);
        });
        (sink, events)
    }

    #[test]
    fn registration_trusts_saved_and_accepted_fingerprints() {
        let config = Config {
            nickname: "tyler".into(),
            ..Config::default()
        };
        let server = ServerConfig {
            fingerprint: " AB:CD ".into(),
            ..server()
        };
        let reg = Registration::new(&config, &server, &["12:34".into()]).unwrap();
        assert!(reg.trusts("ab:cd"));
        assert!(reg.trusts("12:34"));
        assert!(!reg.trusts("56:78"));
        assert!(
            !Registration::new(&config, &self::server(), &[])
                .unwrap()
                .trusts("")
        );
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
            Self::start_with(Registration {
                password: password.into(),
                connect_commands: connect_commands.iter().map(|s| s.to_string()).collect(),
                ..test_reg()
            })
        }

        fn start_with(reg: Registration) -> Self {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (sink, events) = channel_sink();
            let session = tokio::spawn(session(1, reg, client, sink));
            let (read, write) = tokio::io::split(server);
            Self {
                lines: BufReader::new(read).lines(),
                write,
                events,
                session,
            }
        }

        async fn send(&mut self, line: &str) {
            self.write
                .write_all(format!("{line}\r\n").as_bytes())
                .await
                .unwrap();
        }

        /// The next line the client sent.
        async fn recv(&mut self) -> String {
            self.lines
                .next_line()
                .await
                .unwrap()
                .expect("client closed the connection")
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
            fake.send("CAP * LS :").await;
            assert_eq!(fake.recv().await, "CAP END");
            fake.skip_message().await;
            fake
        }
    }

    #[tokio::test]
    async fn cap_without_sasl_and_early_welcome() {
        let mut fake = Fake::start("", &[]);
        assert_eq!(fake.recv().await, "CAP LS 302");
        assert_eq!(fake.recv().await, "NICK tyler");
        fake.recv().await;
        fake.send(":s 001 tyler :Welcome").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
    }
    #[tokio::test(start_paused = true)]
    async fn cap_stall_is_bounded() {
        let mut fake = Fake::start("", &[]);
        while !fake.recv().await.starts_with("USER") {}
        tokio::time::advance(Duration::from_secs(31)).await;
        let (reason, retry) = fake.session.await.unwrap();
        assert!(retry && reason.contains("negotiation"));
    }

    #[tokio::test]
    async fn playback_ctcp_is_quiet_but_live_ctcp_replies() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("BATCH +h chathistory #test").await;
        fake.send("@batch=h :bob!u@h PRIVMSG #test :\x01PING old\x01")
            .await;
        fake.send("BATCH -h").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
        fake.send(":bob!u@h PRIVMSG #test :\x01PING live\x01").await;
        assert_eq!(fake.recv().await, "NOTICE bob :\x01PING live\x01");
    }

    #[tokio::test]
    async fn bouncer_bind_follows_authentication_before_end() {
        let mut fake = Fake::start_with(Registration {
            sasl_username: "account".into(),
            sasl_password: "secret".into(),
            options: SessionOptions {
                attempt: 7,
                bind_network: Some("42".into()),
            },
            ..test_reg()
        });
        while !fake.recv().await.starts_with("USER") {}
        fake.send("CAP * LS :sasl=PLAIN soju.im/bouncer-networks")
            .await;
        assert_eq!(fake.recv().await, "CAP REQ :sasl soju.im/bouncer-networks");
        fake.send("CAP * ACK :sasl soju.im/bouncer-networks").await;
        assert_eq!(fake.recv().await, "AUTHENTICATE PLAIN");
        fake.send("AUTHENTICATE +").await;
        fake.recv().await;
        fake.send(":s 903 tyler :Authenticated").await;
        assert_eq!(fake.recv().await, "BOUNCER BIND 42");
        assert_eq!(fake.recv().await, "CAP END");
    }

    async fn bouncer_session(bind: Option<&str>) -> Fake {
        let mut fake = Fake::start_with(Registration {
            discover_networks: true,
            sasl_username: "account".into(),
            sasl_password: "secret".into(),
            connect_commands: vec!["JOIN #configured".into()],
            options: SessionOptions {
                attempt: 7,
                bind_network: bind.map(str::to_owned),
            },
            ..test_reg()
        });
        while !fake.recv().await.starts_with("USER") {}
        fake.send("CAP * LS :sasl=PLAIN soju.im/bouncer-networks")
            .await;
        fake.recv().await;
        fake.send("CAP * ACK :sasl soju.im/bouncer-networks").await;
        assert_eq!(fake.recv().await, "AUTHENTICATE PLAIN");
        fake.send("AUTHENTICATE +").await;
        fake.recv().await;
        fake
    }

    #[tokio::test(start_paused = true)]
    async fn bouncer_child_auth_failure_is_terminal() {
        let mut fake = bouncer_session(Some("42")).await;
        fake.send(":s 904 tyler :Authentication failed").await;
        let (reason, retry) = fake.session.await.unwrap();
        assert!(
            !retry,
            "binding prerequisites must be a terminal failure: {reason}"
        );
        assert!(reason.contains("binding") || reason.contains("authentication"));
    }

    #[tokio::test(start_paused = true)]
    async fn bouncer_child_without_cap_never_becomes_unbound() {
        let mut fake = Fake::start_with(Registration {
            options: SessionOptions {
                attempt: 9,
                bind_network: Some("42".into()),
            },
            ..test_reg()
        });
        while !fake.recv().await.starts_with("USER") {}
        fake.send(":s 421 tyler CAP :Unknown command").await;
        let (reason, retry) = fake.session.await.unwrap();
        assert!(
            !retry,
            "child must stop immediately if CAP is absent: {reason}"
        );
    }

    #[tokio::test]
    async fn bouncer_parent_auth_failure_falls_back_to_direct() {
        let mut fake = bouncer_session(None).await;
        fake.send(":s 904 tyler :Authentication failed").await;
        assert_eq!(fake.recv().await, "CAP END");
        fake.send(":s 001 tyler :Welcome").await;
        fake.send(":s 376 tyler :End of MOTD").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "JOIN #configured");
        assert_eq!(fake.recv().await, "PONG fence");
        let mut direct = false;
        while let Ok(e) = fake.events.try_recv() {
            if matches!(e, NetEvent::Role(ConnectionRole::Direct)) {
                direct = true;
            }
            assert!(!matches!(e, NetEvent::Role(ConnectionRole::Control)));
        }
        assert!(direct);
    }

    #[tokio::test]
    async fn bouncer_manual_bound_session_preserves_commands() {
        let mut fake = bouncer_session(None).await;
        fake.send(":s 903 tyler :Authenticated").await;
        assert_eq!(fake.recv().await, "CAP END");
        fake.send(":s 001 tyler :Welcome").await;
        fake.send(":s 005 tyler BOUNCER_NETID=42 :supported").await;
        fake.send(":s 376 tyler :End of MOTD").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "JOIN #configured");
        assert_eq!(fake.recv().await, "PONG fence");
        let mut bound = false;
        while let Ok(e) = fake.events.try_recv() {
            bound |= matches!(e, NetEvent::Role(ConnectionRole::Bound(id)) if id == "42");
        }
        assert!(bound);
    }

    #[tokio::test]
    async fn bouncer_rejected_early_welcome_is_not_delivered() {
        let mut fake = bouncer_session(Some("42")).await;
        fake.send(":s 001 tyler :Welcome before login").await;
        assert!(!fake.session.await.unwrap().1);
        while let Ok(e) = fake.events.try_recv() {
            assert!(!matches!(
                e,
                NetEvent::Message(Message {
                    command: Command::Response(Response::RPL_WELCOME, _),
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn authentication_payload_and_bouncer_failures_never_reach_events() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("AUTHENTICATE secret-auth-payload").await;
        fake.send("AUTHENTICATE secret-payload extra").await;
        fake.send(":s 900 tyler account :secret-auth-status").await;
        fake.send("FAIL BOUNCER INVALID_ATTRIBUTE ADDNETWORK * pass=secret-pass :bad secret-pass")
            .await;
        fake.send(":s 461 tyler BOUNCER :bad pass=secret-pass")
            .await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
        while let Ok(e) = fake.events.try_recv() {
            let text = format!("{e:?}");
            assert!(
                !text.contains("secret-"),
                "credential-bearing event: {text}"
            );
        }
    }

    #[tokio::test]
    async fn role_waits_for_isupport_but_direct_commands_run_on_welcome() {
        let mut fake = Fake::registered("", &["JOIN #configured"]).await;
        fake.send(":s 001 tyler :Welcome").await;
        fake.send("PING :before-isupport").await;
        assert_eq!(fake.recv().await, "JOIN #configured");
        assert_eq!(fake.recv().await, "PONG before-isupport");
        while let Ok(e) = fake.events.try_recv() {
            assert!(
                !matches!(e, NetEvent::Role(_)),
                "role ready before ISUPPORT/MOTD"
            );
        }
        fake.send(":s 005 tyler CHATHISTORY=50 MSGREFTYPES=timestamp :supported")
            .await;
        fake.send(":s 376 tyler :End MOTD").await;
        fake.send("PING :ready").await;
        assert_eq!(fake.recv().await, "PONG ready");
        let mut ready = false;
        while let Ok(e) = fake.events.try_recv() {
            ready |= matches!(e, NetEvent::Role(ConnectionRole::Direct));
        }
        assert!(ready);
    }

    #[tokio::test]
    async fn bouncer_history_snapshot_cannot_create_live_networks() {
        let mut fake = bouncer_session(None).await;
        fake.send(":s 903 tyler :Authenticated").await;
        assert_eq!(fake.recv().await, "CAP END");
        fake.send(":s 001 tyler :Welcome").await;
        fake.send(":s 376 tyler :End MOTD").await;
        assert_eq!(fake.recv().await, "BOUNCER LISTNETWORKS");
        while fake.events.try_recv().is_ok() {}
        fake.send("BATCH +old chathistory #test").await;
        fake.send("@batch=old BATCH +net soju.im/bouncer-networks")
            .await;
        fake.send("@batch=net BOUNCER NETWORK 42 name=Historical;pass=secret")
            .await;
        fake.send("@batch=old BATCH -net").await;
        fake.send("BATCH -old").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
        while let Ok(e) = fake.events.try_recv() {
            assert!(!matches!(
                e,
                NetEvent::Network(_) | NetEvent::NetworkSnapshot(_)
            ));
        }
    }

    #[tokio::test]
    async fn canceled_network_snapshot_never_removes_existing_networks() {
        let mut fake = bouncer_session(None).await;
        fake.send(":s 903 tyler :Authenticated").await;
        assert_eq!(fake.recv().await, "CAP END");
        fake.send(":s 001 tyler :Welcome").await;
        fake.send(":s 376 tyler :End MOTD").await;
        assert_eq!(fake.recv().await, "BOUNCER LISTNETWORKS");
        for line in [
            "BATCH +initial soju.im/bouncer-networks",
            "@batch=initial BOUNCER NETWORK 42 name=First",
            "BATCH -initial",
            "BATCH +partial soju.im/bouncer-networks",
            "@batch=partial BOUNCER NETWORK 43 name=Second",
        ] {
            fake.send(line).await;
        }
        fake.send("PING :before-cancel").await;
        assert_eq!(fake.recv().await, "PONG before-cancel");
        while fake.events.try_recv().is_ok() {}
        fake.send("CAP * DEL :soju.im/bouncer-networks").await;
        fake.send("BATCH -partial").await;
        fake.send("PING :after-cancel").await;
        assert_eq!(fake.recv().await, "PONG after-cancel");
        while let Ok(event) = fake.events.try_recv() {
            assert!(
                !matches!(
                    event,
                    NetEvent::Network(NetworkUpdate::Remove(_)) | NetEvent::NetworkSnapshot(_)
                ),
                "canceled snapshot inferred deletion"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn role_detection_falls_back_five_seconds_after_welcome_without_motd() {
        let mut fake = Fake::registered("", &[]).await;
        let start = Instant::now();
        fake.send(":s 001 tyler :Welcome").await;
        fake.skip_message().await;
        assert!(matches!(
            fake.event().await,
            NetEvent::Role(ConnectionRole::Direct)
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn history_failure_is_not_delivered_to_live_error_speech() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("FAIL CHATHISTORY INVALID_TARGET LATEST bob :History unavailable")
            .await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
        while let Ok(e) = fake.events.try_recv() {
            assert!(
                !matches!(e, NetEvent::Message(Message {command: Command::Raw(ref cmd, _), ..}) if cmd == "FAIL")
            );
        }
    }

    #[tokio::test]
    async fn credentials_are_sent_once_even_for_duplicate_challenges() {
        let mut fake = bouncer_session(None).await;
        fake.send("AUTHENTICATE +").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
    }

    #[tokio::test(start_paused = true)]
    async fn bouncer_wrong_bound_network_is_terminal() {
        let mut fake = bouncer_session(Some("42")).await;
        fake.send(":s 903 tyler :Authenticated").await;
        assert_eq!(fake.recv().await, "BOUNCER BIND 42");
        assert_eq!(fake.recv().await, "CAP END");
        fake.send(":s 001 tyler :Welcome").await;
        assert_eq!(fake.recv().await, "JOIN #configured");
        fake.send(":s 005 tyler BOUNCER_NETID=43 :supported").await;
        let (reason, retry) = fake.session.await.unwrap();
        assert!(!retry && reason.contains("different network"));
    }

    #[tokio::test]
    async fn pass_only_bouncer_explains_missing_network_sessions_once() {
        for (discover, bound, offered, expected) in [
            (true, false, true, true),
            (true, true, true, false),
            (false, false, true, false),
            (true, false, false, false),
        ] {
            let mut fake = Fake::start_with(Registration {
                password: "test-server-password".into(),
                discover_networks: discover,
                ..test_reg()
            });
            while !fake.recv().await.starts_with("USER") {}
            fake.send(if offered {
                "CAP * LS :sasl=PLAIN soju.im/bouncer-networks"
            } else {
                "CAP * LS :"
            })
            .await;
            assert_eq!(fake.recv().await, "CAP END");
            fake.send(":soju 001 tyler :Welcome to soju").await;
            if bound {
                fake.send(":soju 005 tyler BOUNCER_NETID=42 :supported")
                    .await;
            }
            fake.send(":soju 376 tyler :End MOTD").await;
            fake.send(":soju 376 tyler :Repeated end MOTD").await;
            fake.send("PING :fence").await;
            assert_eq!(fake.recv().await, "PONG fence");
            let mut notices = Vec::new();
            while let Ok(event) = fake.events.try_recv() {
                if let NetEvent::Notice(text) = event {
                    notices.push(text);
                }
            }
            assert_eq!(notices.len(), usize::from(expected));
            if expected {
                assert!(notices[0].contains("Connected to the bouncer account"));
                assert!(notices[0].contains("SASL username and password"));
                assert!(!notices[0].contains("test-server-password"));
            }
        }
    }

    #[tokio::test]
    async fn registers() {
        let mut fake = Fake::start("user/net:pass", &[]);
        assert_eq!(fake.recv().await, "CAP LS 302");
        assert_eq!(fake.recv().await, "PASS user/net:pass");
        assert_eq!(fake.recv().await, "NICK tyler");
        assert_eq!(fake.recv().await, "USER tspivey 0 * :Tyler S");
        assert!(matches!(fake.event().await, NetEvent::Connected(_)));
        assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "tyler"));
    }

    #[tokio::test]
    async fn no_pass_without_password() {
        let mut fake = Fake::start("", &[]);
        assert_eq!(fake.recv().await, "CAP LS 302");
        assert_eq!(fake.recv().await, "NICK tyler");
    }

    #[test]
    fn sasl_username_falls_back_to_nick() {
        let config = Config {
            nickname: "tyler".into(),
            ..Config::default()
        };
        let reg = Registration::new(&config, &server(), &[]).unwrap();
        assert_eq!(reg.sasl_username, "tyler");
        assert_eq!(reg.sasl_password, "");
        let server = ServerConfig {
            sasl_username: " account ".into(),
            sasl_password: "pw".into(),
            ..server()
        };
        let reg = Registration::new(&config, &server, &[]).unwrap();
        assert_eq!(reg.sasl_username, "account");
        assert_eq!(reg.sasl_password, "pw");
    }

    /// A session logging in as `account`, read up to the start of capability negotiation.
    async fn sasl_session(password: &str) -> Fake {
        let mut fake = Fake::start_with(Registration {
            sasl_username: "account".into(),
            sasl_password: password.into(),
            ..test_reg()
        });
        assert_eq!(fake.recv().await, "CAP LS 302");
        assert_eq!(fake.recv().await, "NICK tyler");
        assert_eq!(fake.recv().await, "USER tspivey 0 * :Tyler S");
        assert!(matches!(fake.event().await, NetEvent::Connected(_)));
        assert!(matches!(fake.event().await, NetEvent::NickChanged(_)));
        fake
    }

    /// Skips events up to the next Error and returns its text.
    async fn next_error(fake: &mut Fake) -> String {
        loop {
            if let NetEvent::Error(e) = fake.event().await {
                return e;
            }
        }
    }

    #[tokio::test]
    async fn sasl_login() {
        let mut fake = sasl_session("secret").await;
        // A multi-line list, with sasl on the last line.
        fake.send(":irc.test CAP * LS * :multi-prefix away-notify")
            .await;
        fake.send(":irc.test CAP * LS :sasl=EXTERNAL,PLAIN server-time")
            .await;
        assert_eq!(fake.recv().await, "CAP REQ :sasl server-time multi-prefix");
        fake.send(":irc.test CAP tyler ACK :sasl server-time multi-prefix")
            .await;
        assert_eq!(fake.recv().await, "AUTHENTICATE PLAIN");
        fake.send("AUTHENTICATE +").await;
        assert_eq!(
            fake.recv().await,
            format!("AUTHENTICATE {}", BASE64.encode("\0account\0secret"))
        );
        fake.send(":irc.test 900 tyler tyler!t@host account :You are now logged in as account")
            .await;
        fake.send(":irc.test 903 tyler :SASL authentication successful")
            .await;
        assert_eq!(fake.recv().await, "CAP END");
        // Only once.
        fake.send(":irc.test 907 tyler :You have already authenticated")
            .await;
        fake.send("PING :x").await;
        assert_eq!(fake.recv().await, "PONG x");
    }

    #[tokio::test]
    async fn sasl_without_mechanism_list() {
        let mut fake = sasl_session("secret").await;
        fake.send(":irc.test CAP * LS :sasl").await;
        assert_eq!(fake.recv().await, "CAP REQ sasl");
    }

    #[tokio::test]
    async fn sasl_not_offered() {
        let mut fake = sasl_session("secret").await;
        fake.send(":irc.test CAP * LS :multi-prefix sasl-other")
            .await;
        assert_eq!(fake.recv().await, "CAP REQ multi-prefix");
        fake.send("CAP * ACK :multi-prefix").await;
        assert_eq!(fake.recv().await, "CAP END");
        assert!(next_error(&mut fake).await.contains("SASL PLAIN"));
    }

    #[tokio::test]
    async fn sasl_without_plain() {
        let mut fake = sasl_session("secret").await;
        fake.send(":irc.test CAP * LS :sasl=EXTERNAL").await;
        assert_eq!(fake.recv().await, "CAP END");
        assert!(next_error(&mut fake).await.contains("PLAIN"));
    }

    #[tokio::test]
    async fn sasl_refused() {
        let mut fake = sasl_session("secret").await;
        fake.send(":irc.test CAP * LS :sasl").await;
        assert_eq!(fake.recv().await, "CAP REQ sasl");
        fake.send(":irc.test CAP tyler NAK :sasl").await;
        assert_eq!(fake.recv().await, "CAP END");
        assert_eq!(
            next_error(&mut fake).await,
            "The server refused SASL; carrying on without logging in."
        );
    }

    #[tokio::test]
    async fn sasl_failure_carries_on() {
        let mut fake = sasl_session("wrong").await;
        fake.send(":irc.test CAP * LS :sasl").await;
        fake.recv().await;
        fake.send(":irc.test CAP tyler ACK :sasl").await;
        fake.recv().await;
        fake.send("AUTHENTICATE +").await;
        fake.recv().await;
        fake.send(":irc.test 904 tyler :SASL authentication failed")
            .await;
        assert_eq!(fake.recv().await, "CAP END");
        assert_eq!(
            next_error(&mut fake).await,
            "SASL login failed; carrying on without logging in."
        );
        // A second failure numeric doesn't end negotiation again.
        fake.send(":irc.test 904 tyler :SASL authentication failed")
            .await;
        fake.send("PING :x").await;
        assert_eq!(fake.recv().await, "PONG x");
    }

    #[tokio::test]
    async fn sasl_long_credentials_are_split() {
        // "\0account\0" + 291 bytes = 300 bytes = exactly 400 base64 characters.
        let mut fake = sasl_session(&"p".repeat(291)).await;
        fake.send(":irc.test CAP * LS :sasl").await;
        fake.recv().await;
        fake.send(":irc.test CAP tyler ACK :sasl").await;
        fake.recv().await;
        fake.send("AUTHENTICATE +").await;
        assert_eq!(fake.recv().await.len(), "AUTHENTICATE ".len() + 400);
        assert_eq!(fake.recv().await, "AUTHENTICATE +");

        let mut fake = sasl_session(&"p".repeat(300)).await;
        fake.send(":irc.test CAP * LS :sasl").await;
        fake.recv().await;
        fake.send(":irc.test CAP tyler ACK :sasl").await;
        fake.recv().await;
        fake.send("AUTHENTICATE +").await;
        assert_eq!(fake.recv().await.len(), "AUTHENTICATE ".len() + 400);
        let rest = fake.recv().await;
        assert!(
            rest.starts_with("AUTHENTICATE ") && rest != "AUTHENTICATE +",
            "{rest}"
        );
    }

    #[tokio::test]
    async fn welcome_ends_sasl() {
        // A server without CAP registers us anyway; a late 904 then sends nothing.
        let mut fake = sasl_session("secret").await;
        fake.send(":irc.test 421 tyler CAP :Unknown command").await;
        fake.send(":irc.test 001 tyler :Welcome").await;
        fake.send(":irc.test 904 tyler :SASL authentication failed")
            .await;
        fake.send("PING :x").await;
        assert_eq!(fake.recv().await, "PONG x");
    }

    #[tokio::test]
    async fn answers_ping() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("PING :irc.test").await;
        assert_eq!(fake.recv().await, "PONG irc.test");
        assert!(
            matches!(fake.event().await, NetEvent::Message(m) if matches!(m.command, Command::PING(..)))
        );
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
        assert_eq!(next_error(&mut fake).await, "Invalid connect command.");
    }

    #[tokio::test]
    async fn alt_nicks_then_gives_up() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":irc.test 433 * tyler :Nickname is already in use")
            .await;
        assert_eq!(fake.recv().await, "NICK tyler_");
        fake.skip_message().await;
        assert!(matches!(fake.event().await, NetEvent::NickChanged(n) if n == "tyler_"));
        fake.send(":irc.test 433 * tyler_ :Nickname is already in use")
            .await;
        assert_eq!(fake.recv().await, "NICK tyler__");
        fake.send(":irc.test 433 * tyler__ :Nickname is already in use")
            .await;
        assert_eq!(
            fake.session.await.unwrap(),
            ("Every nickname is in use.".into(), false)
        );
    }

    #[tokio::test]
    async fn nick_in_use_after_registration_is_left_to_the_user() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":irc.test 001 tyler :Welcome").await;
        fake.send(":irc.test 433 tyler other :Nickname is already in use")
            .await;
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
        fake.send(":bob!b@host PRIVMSG tyler :\x01VERSION\x01")
            .await;
        assert_eq!(
            fake.recv().await,
            format!(
                "NOTICE bob :\x01VERSION irctest {}\x01",
                env!("CARGO_PKG_VERSION")
            )
        );
        fake.send(":bob!b@host PRIVMSG #test :\x01PING 12345\x01")
            .await;
        assert_eq!(fake.recv().await, "NOTICE bob :\x01PING 12345\x01");
        // Not answered: ACTION, unknown queries, and our own messages.
        fake.send(":bob!b@host PRIVMSG tyler :\x01ACTION waves\x01")
            .await;
        fake.send(":bob!b@host PRIVMSG tyler :\x01FINGER\x01").await;
        fake.send(":tyler!t@host PRIVMSG tyler :\x01VERSION\x01")
            .await;
        fake.send("PING :done").await;
        assert_eq!(fake.recv().await, "PONG done");
    }

    #[tokio::test]
    async fn ctcp_ping_preserves_parameters_including_an_empty_payload() {
        let mut fake = Fake::registered("", &[]).await;
        for (body, reply) in [
            ("\x01PING\x01", "NOTICE bob \x01PING\x01"),
            (
                "\x01PING 123  456 \x01",
                "NOTICE bob :\x01PING 123  456 \x01",
            ),
            (
                "\x01PING no-closing-delimiter",
                "NOTICE bob :\x01PING no-closing-delimiter\x01",
            ),
        ] {
            fake.send(&format!(":bob!u@h PRIVMSG tyler :{body}")).await;
            assert_eq!(fake.recv().await, reply);
        }
    }

    #[tokio::test]
    async fn ctcp_clientinfo_and_time_reply_privately_to_channel_requests() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send(":bob!u@h PRIVMSG #test :\x01CLIENTINFO\x01")
            .await;
        assert_eq!(
            fake.recv().await,
            "NOTICE bob :\x01CLIENTINFO ACTION CLIENTINFO PING TIME VERSION\x01"
        );
        let before = chrono::Utc::now().timestamp();
        fake.send(":bob!u@h PRIVMSG #test :\x01TIME\x01").await;
        let reply: Message = fake.recv().await.parse().unwrap();
        let Command::NOTICE(target, body) = reply.command else {
            panic!("TIME did not reply with NOTICE");
        };
        assert_eq!(target, "bob");
        let (command, time) = parse_ctcp(&body).expect("TIME reply was not framed as CTCP");
        assert_eq!(command, "TIME");
        let time = chrono::DateTime::parse_from_rfc2822(time)
            .unwrap()
            .timestamp();
        assert!((before..=chrono::Utc::now().timestamp()).contains(&time));
    }

    #[tokio::test]
    async fn ctcp_malformed_frames_and_notices_never_trigger_replies() {
        let mut fake = Fake::registered("", &[]).await;
        for line in [
            ":bob!u@h PRIVMSG tyler :\x01PING x\x01\x01VERSION\x01",
            ":bob!u@h PRIVMSG tyler :\x01PING x\x01trailing text",
            ":bob!u@h NOTICE tyler :\x01PING 123\x01",
            ":bob!u@h NOTICE tyler :\x01VERSION\x01",
        ] {
            fake.send(line).await;
        }
        fake.send("PING :ctcp-fence").await;
        assert_eq!(fake.recv().await, "PONG ctcp-fence");
    }

    #[tokio::test]
    async fn survives_bad_lines() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("").await;
        fake.send(":irc.test :oops").await;
        assert!(
            matches!(fake.event().await, NetEvent::Error(e) if e.starts_with("Couldn't parse"))
        );
        fake.write
            .write_all(b":bob!b@host PRIVMSG #test :caf\xe9\r\n")
            .await
            .unwrap();
        match fake.event().await {
            NetEvent::Message(m) => assert_eq!(
                m.command,
                Command::PRIVMSG("#test".into(), "caf\u{FFFD}".into())
            ),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_history_line_never_reparses_its_remainder_as_live_ctcp() {
        let mut fake = Fake::registered("", &[]).await;
        fake.send("BATCH +h chathistory #test").await;
        let prefix = "@batch=h :bob!u@h PRIVMSG #test :";
        let line = format!(
            "{prefix}{}:bob!u@h PRIVMSG #test :\x01PING historical\x01",
            "x".repeat(MAX_LINE - prefix.len())
        );
        fake.send(&line).await;
        fake.send("BATCH -h").await;
        fake.send("PING :fence").await;
        assert_eq!(fake.recv().await, "PONG fence");
        while let Ok(event) = fake.events.try_recv() {
            assert!(!matches!(event, NetEvent::HistoryMessage { .. }));
            assert!(!matches!(
                event,
                NetEvent::Message(Message {
                    command: Command::PRIVMSG(..),
                    ..
                })
            ));
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
        while !fake.recv().await.starts_with("USER") {}
        let NetEvent::Connected(sender) = fake.event().await else {
            panic!()
        };
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
        assert_eq!(
            fake.session.await.unwrap(),
            ("Connection closed.".into(), true)
        );
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
            let fingerprint = fingerprint(
                &native_tls::Certificate::from_pem(cert)
                    .unwrap()
                    .to_der()
                    .unwrap(),
            );
            let identity = native_tls::Identity::from_pkcs8(cert, key).unwrap();
            let acceptor = tokio_native_tls::TlsAcceptor::from(
                native_tls::TlsAcceptor::new(identity).unwrap(),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, lines) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let (acceptor, tx) = (acceptor.clone(), tx.clone());
                    tokio::spawn(async move {
                        // The verifying client hangs up mid-handshake.
                        let Ok(tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        let mut line = String::new();
                        if BufReader::new(tls).read_line(&mut line).await.is_ok() {
                            let _ = tx.send(line.trim_end().to_string());
                        }
                    });
                }
            });
            Self {
                port,
                fingerprint,
                lines,
            }
        }

        /// Runs a connection to this server; returns its events and the result.
        fn connect(
            &self,
            pinned: &str,
            accepted: &[&str],
        ) -> (
            mpsc::UnboundedReceiver<NetEvent>,
            JoinHandle<(String, bool)>,
        ) {
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
    async fn attempt_envelope_survives_certificate_and_final_disconnect() {
        let server = TlsServer::start().await;
        let (tx, mut events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |id, event| {
            let _ = tx.send((id, event));
        });
        let config = Config {
            nickname: "reader".into(),
            ..Config::default()
        };
        let config_server = ServerConfig {
            host: "127.0.0.1".into(),
            port: server.port,
            tls: true,
            ..ServerConfig::default()
        };
        let _task = connect(
            &Handle::current(),
            17,
            &config,
            &config_server,
            &[],
            SessionOptions {
                attempt: 777,
                bind_network: None,
            },
            sink,
        );
        let (id, event) = events.recv().await.unwrap();
        assert_eq!((id, event.attempt), (17, 777));
        let NetEvent::UntrustedCertificate(prompt) = event.event else {
            panic!("missing certificate envelope")
        };
        prompt.answer(false);
        let (id, event) = events.recv().await.unwrap();
        assert_eq!((id, event.attempt), (17, 777));
        assert!(matches!(
            event.event,
            NetEvent::Disconnected { retry: false, .. }
        ));
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
        assert_eq!(server.lines.recv().await.unwrap(), "CAP LS 302");
    }

    #[tokio::test]
    async fn refused_certificate_disconnects_for_good() {
        let server = TlsServer::start().await;
        let (mut events, run) = server.connect("", &[]);
        expect_prompt(&mut events).await.answer(false);
        assert_eq!(
            run.await.unwrap(),
            ("The server's certificate wasn't trusted.".into(), false)
        );
    }

    #[tokio::test]
    async fn trusted_fingerprints_skip_the_prompt() {
        let mut server = TlsServer::start().await;
        let fingerprint = server.fingerprint.to_lowercase();
        for (pinned, accepted) in [
            (fingerprint.as_str(), vec![]),
            ("", vec![fingerprint.as_str()]),
        ] {
            let (mut events, _run) = server.connect(pinned, &accepted);
            assert!(matches!(events.recv().await, Some(NetEvent::Connected(_))));
            assert_eq!(server.lines.recv().await.unwrap(), "CAP LS 302");
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
        let reg = Registration {
            host: "127.0.0.1".into(),
            port,
            tls: true,
            ..test_reg()
        };
        let (sink, mut events) = channel_sink();
        let (reason, retry) = run(1, reg, sink).await;
        assert!(reason.starts_with("TLS handshake failed: "), "{reason}");
        assert!(retry);
        assert!(events.try_recv().is_err());
    }
}

#[cfg(test)]
mod scenarios;
