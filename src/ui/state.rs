//! Application state on the GUI thread: servers, their tabs, routing and typed commands.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use chrono::{DateTime, Local};
use irc_proto::{ChannelExt, ChannelMode, Command, Message, Mode, Prefix, Response};
use irctest::commands::{Input, join_lines, parse_input, parse_join};
use irctest::config::{Config, ServerConfig};
use irctest::format::{
    format_message, is_error_reply, message_time, parse_ctcp, spoken_channel, strip_formatting,
    timestamped,
};
use irctest::history::History;
use irctest::log::{Logger, SERVER_TARGET};
use irctest::net::{self, CertificatePrompt, EventSink, NetEvent, Sender, ServerId};
use irctest::playback::{Deduper, HistoryReason, HistoryRequest, cursor, delivery_policy};
use irctest::route::{Route, irc_eq, irc_lower, route};
use irctest::speech::{Speech, mentions, should_speak};
use irctest::sync_state::{AccountKey, ConversationKey, Cursor, SyncData, Writer};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;
use wxdragon::prelude::*;

use super::cert_dialog::CertificateChoice;
use super::foreground::app_is_foreground;
use super::tab::{Tab, set_page_text};

pub type TabId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Server,
    Channel,
    Query,
}

pub struct ChatTab {
    id: TabId,
    kind: TabKind,
    /// Channel name, nick, or the server's name.
    name: String,
    tab: Tab,
    /// F5: speak new messages in this tab. On by default for channels and queries.
    speech: bool,
    /// Channels: whether we're currently in it.
    joined: bool,
    /// Channels: we were in it when the connection dropped, so join it again on the next 001.
    rejoin: bool,
    /// Channels: the key we joined with, or the one set since (+k). Kept for rejoining; never saved.
    key: Option<String>,
    /// Channels: who's in it, as `irc_lower` nicks. Used to route QUIT and NICK.
    nicks: HashSet<String>,
    /// Lines typed into this tab, for Up/Down.
    history: History,
    deduper: Deduper,
    oldest: Option<Cursor>,
    history_boundary: Option<usize>,
    history_attempt: Option<u64>,
    marker_attempt: Option<u64>,
    first_chat: Option<usize>,
}

pub struct Server {
    id: ServerId,
    attempt: u64,
    parent: Option<ServerId>,
    network_id: Option<String>,
    role: irctest::bouncer::ConnectionRole,
    group: irctest::connections::Group,
    markers: irctest::read_marker::Tracker,
    marker_pending: HashMap<String, String>,
    active_config: Config,
    account_key: AccountKey,
    actual_network: Option<String>,
    ready: bool,
    playback: PlaybackBuffers,
    early_chat: Vec<(String, Message, usize)>,
    deferred_logs: Vec<(String, DateTime<Local>, String, bool)>,
    early_markers: Vec<irctest::read_marker::Update>,
    history_aliases: HashMap<String, String>,
    marker_aliases: HashMap<String, String>,
    manual_suppressed: HashSet<String>,
    network_ids: HashSet<String>,
    ever_registered: bool,
    cold_start: bool,
    history_targets_started: bool,
    config: ServerConfig,
    /// Set once the connection is up; cleared when it drops.
    sender: Option<Sender>,
    /// Set while connecting or connected.
    task: Option<AbortHandle>,
    capabilities: irctest::cap::Capabilities,
    nick: String,
    /// Server tab first, then channels, then queries: the same order as in the notebook.
    tabs: Vec<ChatTab>,
    /// `irc_lower` channels we asked to join, whose tabs get selected when the JOIN arrives.
    pending_select: HashSet<String>,
    /// Keys from JOINs we sent, by `irc_lower` channel, until the JOIN arrives and moves the key
    /// into the channel's tab.
    pending_keys: HashMap<String, String>,
    /// Seen 001 on the current connection.
    registered: bool,
    /// The user disconnected on purpose (/quit, Server → Disconnect): don't reconnect.
    user_quit: bool,
    /// Reconnect attempts since the last successful registration, for the backoff.
    attempts: u32,
    /// One-shot timer for the next automatic reconnect; running while one is pending.
    reconnect_timer: Timer<Notebook>,
    /// Certificate fingerprints the user chose to trust once: kept for reconnects until
    /// irctest exits, but not saved.
    accepted_fingerprints: Vec<String>,
}

pub struct State {
    config: Config,
    sync_data: SyncData,
    writer: Writer,
    sync_error: Option<String>,
    shutting_down: bool,
    runtime: Handle,
    notebook: Notebook,
    sink: EventSink,
    servers: Vec<Server>,
    next_server_id: ServerId,
    next_tab_id: TabId,
    /// The "Not connected" page shown before any server tab exists.
    placeholder: Option<Panel>,
    speech: Speech,
    /// Whether the "speech isn't available" note has been shown (once, in the first server tab).
    speech_error_shown: bool,
    logger: Logger,
    /// Whether a log write has failed and been reported (once; later failures are silent).
    log_error_shown: bool,
    /// For binding input boxes of new tabs back to this state.
    me: Weak<RefCell<State>>,
}

/// Resolve a reply to the tab that owns its original request, including renames.
fn request_target(aliases: &mut HashMap<String, String>, target: String, terminal: bool) -> String {
    let destination = aliases.get(&irc_lower(&target)).cloned().unwrap_or(target);
    if terminal {
        retire_target(aliases, &destination);
    }
    destination
}
fn retire_target(aliases: &mut HashMap<String, String>, target: &str) {
    aliases.retain(|_, destination| !irc_eq(destination, target));
}

fn marker_acknowledges(requested: &str, received: Option<&str>) -> bool {
    let Some(received) = received.and_then(|time| irctest::sync_state::timestamp(time).ok()) else {
        return false;
    };
    irctest::sync_state::timestamp(requested).is_ok_and(|requested| received >= requested)
}
fn login_key(config: &Config, server: &ServerConfig) -> AccountKey {
    let mut key = AccountKey::from_config(config, server);
    key.name.clear();
    if !server.sasl_password.is_empty() {
        key.identity = format!(
            "sasl:{}",
            if server.sasl_username.trim().is_empty() {
                config.effective_nick(server)
            } else {
                server.sasl_username.trim()
            }
        );
    }
    key
}
// Soju's documented username syntax is account[/network][@client]. This
// comparison is only used after Control and Bound roles confirm a bouncer;
// persistent AccountKey values deliberately keep the complete configured identity.
fn bouncer_login_key(config: &Config, server: &ServerConfig) -> AccountKey {
    let mut key = login_key(config, server);
    let identity = if server.sasl_password.is_empty() {
        config.effective_username(server)
    } else if server.sasl_username.trim().is_empty() {
        config.effective_nick(server)
    } else {
        server.sasl_username.trim()
    };
    key.identity = format!(
        "bouncer:{}",
        identity.split(['/', '@']).next().unwrap_or(identity)
    );
    key
}
fn update_page_oldest(oldest: &mut Option<Cursor>, reason: HistoryReason, first: Option<Cursor>) {
    let Some(first) = first else {
        return;
    };
    let Ok(time) = irctest::sync_state::timestamp(&first.time) else {
        return;
    };
    if oldest
        .as_ref()
        .and_then(|oldest| irctest::sync_state::timestamp(&oldest.time).ok())
        .is_none_or(|oldest| time < oldest || (reason == HistoryReason::Older && time == oldest))
    {
        *oldest = Some(first);
    }
}
fn single_line(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}
fn displayed_chat(message: &Message) -> bool {
    match &message.command {
        Command::PRIVMSG(_, text) | Command::NOTICE(_, text) => {
            parse_ctcp(text).is_none_or(|(kind, _)| kind == "ACTION")
        }
        _ => false,
    }
}
fn trim_anchor(anchor: &mut Option<usize>, removed: usize) {
    if let Some(anchor) = anchor {
        *anchor = anchor.saturating_sub(removed);
    }
}
fn merge_ids(existing: &mut Vec<String>, incoming: impl IntoIterator<Item = String>) {
    for id in incoming {
        existing.retain(|old| *old != id);
        existing.push(id);
    }
    if existing.len() > 256 {
        existing.drain(..existing.len() - 256);
    }
}
fn merge_conversation(
    into: &mut irctest::sync_state::ConversationState,
    from: irctest::sync_state::ConversationState,
) {
    if let Some(cursor) = from.received {
        into.observe(cursor);
    }
    if let Some(read) = from.read {
        let _ = into.mark_read(&read);
    }
    merge_ids(&mut into.recent_ids, from.recent_ids);
}
#[derive(Default)]
struct BufferedPlayback {
    messages: Vec<(Option<String>, Message)>,
    boundaries: HashMap<String, usize>,
    bytes: usize,
}
#[derive(Default)]
struct PlaybackBuffers {
    batches: HashMap<String, BufferedPlayback>,
    messages: usize,
    bytes: usize,
}
impl PlaybackBuffers {
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn push(
        &mut self,
        batch: &str,
        target: Option<String>,
        message: Message,
    ) -> Result<(), String> {
        let bytes = message.to_string().len();
        if self.messages >= 1000 || self.bytes + bytes > 4 * 1024 * 1024 {
            return Err("Historical playback buffer limit exceeded.".into());
        }
        let entry = self
            .batches
            .get_mut(batch)
            .ok_or("Historical playback has no owning batch.")?;
        entry.messages.push((target, message));
        entry.bytes += bytes;
        self.messages += 1;
        self.bytes += bytes;
        Ok(())
    }
    fn take(&mut self, batch: &str) -> Option<BufferedPlayback> {
        let batch = self.batches.remove(batch)?;
        self.messages -= batch.messages.len();
        self.bytes -= batch.bytes;
        Some(batch)
    }
    fn trim(&mut self, target: &str, count: usize) {
        for batch in self.batches.values_mut() {
            if let Some(at) = batch.boundaries.get_mut(target) {
                *at = at.saturating_sub(count);
            }
        }
    }
}

impl State {
    pub fn new(
        config: Config,
        runtime: Handle,
        notebook: Notebook,
        sink: EventSink,
        placeholder: Panel,
        sync_data: SyncData,
        writer: Writer,
        sync_error: Option<String>,
    ) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|me| {
            RefCell::new(State {
                config,
                sync_data,
                writer,
                sync_error,
                shutting_down: false,
                runtime,
                notebook,
                sink,
                servers: Vec::new(),
                next_server_id: 1,
                next_tab_id: 1,
                placeholder: Some(placeholder),
                speech: Speech::new(),
                speech_error_shown: false,
                logger: Logger::default(),
                log_error_shown: false,
                me: me.clone(),
            })
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        for server in &mut self.servers {
            server.user_quit = true;
            server.attempt += 1;
            server.ready = false;
            server.reconnect_timer.stop();
            let _ = server.group.disconnect();
            if let Some(task) = server.task.take() {
                task.abort();
            }
            server.sender = None;
            server.playback.clear();
            server.early_chat.clear();
            server.early_markers.clear();
            server.deferred_logs.clear();
        }
        self.logger.close_all();
    }
    fn reset_conversations(&mut self, s: usize) {
        self.servers[s].markers = Default::default();
        self.servers[s].marker_pending.clear();
        self.servers[s].playback.clear();
        let cold = self.servers[s].cold_start;
        for t in 1..self.servers[s].tabs.len() {
            let target = self.servers[s].tabs[t].name.clone();
            let key = self.conversation_key(s, &target);
            let state = self
                .sync_data
                .conversations
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let tab = &mut self.servers[s].tabs[t];
            tab.deduper = Deduper::new(if cold { &[] } else { &state.recent_ids });
            tab.oldest = None;
            tab.history_boundary = None;
            tab.history_attempt = None;
            tab.marker_attempt = None;
            tab.first_chat = None;
            let _ = self.servers[s]
                .markers
                .restore(&target, state.read.as_deref());
            self.update_unread(s, &target);
        }
    }
    fn append_output(&mut self, s: usize, t: usize, text: &str) {
        let removed = self.servers[s].tabs[t].tab.append_line(text);
        if removed > 0 {
            let target = irc_lower(&self.servers[s].tabs[t].name);
            let tab = &mut self.servers[s].tabs[t];
            trim_anchor(&mut tab.first_chat, removed);
            trim_anchor(&mut tab.history_boundary, removed);
            self.servers[s].playback.trim(&target, removed);
            for (name, _, at) in &mut self.servers[s].early_chat {
                if irc_eq(name, &target) {
                    *at = at.saturating_sub(removed);
                }
            }
        }
    }
    fn shift_history_positions(
        &mut self,
        s: usize,
        t: usize,
        at: usize,
        inserted: usize,
        trimmed: usize,
    ) {
        let shift = |slot: &mut Option<usize>| {
            if let Some(slot) = slot {
                if *slot >= at {
                    *slot += inserted;
                }
                *slot = slot.saturating_sub(trimmed);
            }
        };
        let target = irc_lower(&self.servers[s].tabs[t].name);
        let tab = &mut self.servers[s].tabs[t];
        shift(&mut tab.first_chat);
        shift(&mut tab.history_boundary);
        for batch in self.servers[s].playback.batches.values_mut() {
            if let Some(boundary) = batch.boundaries.get_mut(&target) {
                if *boundary >= at {
                    *boundary += inserted;
                }
                *boundary = boundary.saturating_sub(trimmed);
            }
        }
    }
    fn refresh_contexts(&mut self, s: usize) {
        self.start_history_targets(s);
        let tabs: Vec<_> = self.servers[s]
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.kind == TabKind::Query || tab.joined)
            .map(|(t, _)| t)
            .collect();
        for t in tabs {
            self.request_context(s, t);
        }
    }
    /// New settings or server list. Servers already connected keep their old settings until
    /// they reconnect.
    pub fn set_config(&mut self, config: Config) {
        if !config.log {
            self.logger.close_all();
        }
        self.config = config;
    }

    /// Connects every server marked auto-connect.
    pub fn connect_auto(&mut self) {
        let servers: Vec<ServerConfig> = self
            .config
            .servers
            .iter()
            .filter(|s| s.auto_connect)
            .cloned()
            .collect();
        for server in servers {
            self.connect(server);
        }
        // Each new server tab took the selection; start on the first one instead.
        if !self.servers.is_empty() {
            self.select_tab(0, 0);
        }
    }

    /// Connects to a server (user's request), reusing its tabs (matched by name) if it had them.
    pub fn connect(&mut self, server_config: ServerConfig) {
        let s = match self
            .servers
            .iter()
            .position(|s| s.parent.is_none() && irc_eq(&s.config.name, &server_config.name))
        {
            Some(s) => {
                self.select_tab(s, 0);
                s
            }
            None => self.add_server(server_config.clone(), true),
        };
        if self.servers[s].task.is_some() {
            self.line(s, 0, "Already connected.");
            return;
        }
        self.servers[s].group.reconnect();
        self.servers[s].config = server_config;
        self.reset_reconnect(s);
        self.start(s);
    }

    /// Opens the connection with the server's current settings, unless one is already open.
    fn start(&mut self, s: usize) {
        if self.shutting_down || self.servers[s].task.is_some() {
            return;
        }
        let active_config = if let Some(parent) = self.servers[s].parent {
            let Some(parent) = self.servers.iter().position(|p| p.id == parent) else {
                return;
            };
            self.servers[parent].active_config.clone()
        } else {
            self.config.clone()
        };
        let account = AccountKey::from_config(&active_config, &self.servers[s].config);
        let changed = account != self.servers[s].account_key;
        if changed {
            if self.servers[s].parent.is_none() {
                self.servers[s].ready = false;
                let actions = self.servers[s].group.disconnect();
                self.child_actions(s, actions);
                self.servers[s].group = Default::default();
                self.servers[s].manual_suppressed.clear();
                self.servers[s].network_ids.clear();
            }
            if self.servers[s].parent.is_none() {
                self.servers[s].accepted_fingerprints.clear();
            }
            self.servers[s].account_key = account;
            self.servers[s].actual_network = self.servers[s].network_id.clone();
            self.servers[s].ever_registered = false;
            self.servers[s].cold_start = true;
            for tab in &mut self.servers[s].tabs {
                tab.rejoin = false;
                tab.joined = false;
                tab.key = None;
                tab.nicks.clear();
            }
            self.reset_conversations(s);
        }
        let server = &mut self.servers[s];
        server.active_config = active_config;
        server.attempt += 1;
        server.ready = false;
        server.role = Default::default();
        server.early_chat.clear();
        server.early_markers.clear();
        server.history_aliases.clear();
        server.marker_aliases.clear();
        server.deferred_logs.clear();
        server.playback.clear();
        server.cold_start = !server.ever_registered;
        server.history_targets_started = false;
        let nick = server
            .active_config
            .effective_nick(&server.config)
            .to_string();
        let text = format!(
            "Connecting to {}:{} as {nick}...",
            server.config.host, server.config.port
        );
        server.task = Some(net::connect(
            &self.runtime,
            server.id,
            &server.active_config,
            &server.config,
            &server.accepted_fingerprints,
            net::SessionOptions {
                attempt: server.attempt,
                bind_network: server.network_id.clone(),
            },
            self.sink.clone(),
        ));
        self.line(s, 0, &text);
    }

    /// The user is taking charge (connect, reconnect): forget any earlier quit and backoff.
    fn reset_reconnect(&mut self, s: usize) {
        let server = &mut self.servers[s];
        server.user_quit = false;
        server.attempts = 0;
        server.reconnect_timer.stop();
    }

    /// The server's settings as they are now in the config (it may have been edited since it
    /// connected), falling back to the settings it last used.
    fn refresh_server_config(&mut self, s: usize) {
        if let Some(parent) = self.servers[s].parent {
            if let Some(p) = self.servers.iter().position(|p| p.id == parent) {
                self.servers[s].config = self.servers[p].config.clone();
                self.servers[s].config.discover_networks = false;
                self.servers[s].accepted_fingerprints =
                    self.servers[p].accepted_fingerprints.clone();
            }
            return;
        }
        let name = &self.servers[s].config.name;
        if let Some(config) = self
            .config
            .servers
            .iter()
            .find(|c| irc_eq(&c.name, name))
            .cloned()
        {
            self.servers[s].config = config;
        }
    }

    /// Server → Reconnect: drop the selected tab's server connection (if any) and connect again
    /// now, with the latest settings.
    pub fn reconnect_current(&mut self) -> Result<(), String> {
        let (s, _) = self
            .current_tab()
            .ok_or("There's no server to reconnect.")?;
        self.reset_reconnect(s);
        if let Some(task) = self.servers[s].task.take() {
            task.abort();
            self.servers[s].attempt += 1;
            self.mark_disconnected(s, "Disconnected: reconnecting.");
        }
        self.refresh_server_config(s);
        if let (Some(parent), Some(id)) =
            (self.servers[s].parent, self.servers[s].network_id.clone())
        {
            let p = self
                .servers
                .iter()
                .position(|p| p.id == parent)
                .ok_or("The parent account is unavailable.")?;
            self.servers[p].manual_suppressed.remove(&id);
            let actions = self.servers[p].group.suppress(&id);
            self.child_actions(p, actions);
            let actions = self.servers[p].group.resume(&id);
            self.child_actions(p, actions);
        } else {
            self.servers[s].group.reconnect();
            self.start(s);
        }
        Ok(())
    }

    /// Waits before reconnecting: 5 seconds, doubling each attempt, at most 5 minutes.
    fn schedule_reconnect(&mut self, s: usize) {
        let server = &mut self.servers[s];
        let delay = net::reconnect_delay(server.attempts);
        server.attempts += 1;
        server.reconnect_timer.start(delay.as_millis() as i32, true);
        self.line(
            s,
            0,
            &format!("Reconnecting in {} seconds...", delay.as_secs()),
        );
    }

    /// The reconnect timer fired.
    fn reconnect_due(&mut self, id: ServerId) {
        let Some(s) = self.servers.iter().position(|s| s.id == id) else {
            return;
        };
        if self.shutting_down || self.servers[s].user_quit || self.servers[s].task.is_some() {
            return;
        }
        self.refresh_server_config(s);
        if let (Some(parent), Some(id)) =
            (self.servers[s].parent, self.servers[s].network_id.clone())
        {
            if let Some(p) = self.servers.iter().position(|p| p.id == parent) {
                let actions = self.servers[p].group.registration_due(&id);
                self.child_actions(p, actions);
            }
        } else {
            self.start(s);
        }
    }

    /// Disconnects the server of the selected tab: QUIT if connected, cancels a connect still
    /// in progress, or cancels a pending reconnect. Errors are messages for the user.
    pub fn disconnect_current(&mut self) -> Result<(), String> {
        let (s, _) = self
            .current_tab()
            .ok_or("There's no server to disconnect.")?;
        let child = self.servers[s].parent.is_some();
        self.servers[s].user_quit = true;
        self.user_disconnect_group(s);
        if child {
            return Ok(());
        }
        let server = &mut self.servers[s];
        server.user_quit = true;
        if server.task.is_none() && server.reconnect_timer.is_running() {
            server.reconnect_timer.stop();
            self.line(s, 0, "Reconnect cancelled.");
            return Ok(());
        }
        if let Some(sender) = &server.sender {
            sender
                .send_quit("")
                .map_err(|e| format!("Couldn't send QUIT: {e}"))?;
            self.line(s, 0, "Disconnecting...");
            Ok(())
        } else if let Some(task) = server.task.take() {
            // Still connecting: the task is killed, so no Disconnected event will come.
            task.abort();
            self.servers[s].attempt += 1;
            self.mark_disconnected(s, "Connection attempt cancelled.");
            Ok(())
        } else {
            Err(format!("{} isn't connected.", server.config.name))
        }
    }

    fn add_server(&mut self, config: ServerConfig, select: bool) -> usize {
        if let Some(placeholder) = self.placeholder.take() {
            self.notebook.remove_page(0);
            placeholder.destroy();
        }
        let id = self.next_server_id;
        self.next_server_id += 1;
        let label = if config.name.is_empty() {
            config.host.clone()
        } else {
            config.name.clone()
        };
        let reconnect_timer = Timer::new(&self.notebook);
        let me = self.me.clone();
        reconnect_timer.on_tick(move |_| {
            if let Some(state) = me.upgrade() {
                state.borrow_mut().reconnect_due(id);
            }
        });
        self.servers.push(Server {
            id,
            attempt: 0,
            parent: None,
            network_id: None,
            role: Default::default(),
            group: Default::default(),
            markers: Default::default(),
            marker_pending: HashMap::new(),
            active_config: self.config.clone(),
            account_key: AccountKey::from_config(&self.config, &config),
            actual_network: None,
            ready: false,
            playback: Default::default(),
            early_chat: Vec::new(),
            deferred_logs: Vec::new(),
            early_markers: Vec::new(),
            history_aliases: HashMap::new(),
            marker_aliases: HashMap::new(),
            manual_suppressed: HashSet::new(),
            network_ids: HashSet::new(),
            ever_registered: false,
            cold_start: true,
            history_targets_started: false,
            config,
            sender: None,
            task: None,
            capabilities: Default::default(),
            nick: String::new(),
            tabs: Vec::new(),
            pending_select: HashSet::new(),
            pending_keys: HashMap::new(),
            registered: false,
            user_quit: false,
            attempts: 0,
            reconnect_timer,
            accepted_fingerprints: Vec::new(),
        });
        let s = self.servers.len() - 1;
        let t = self.insert_tab(s, TabKind::Server, &label, select);
        if select {
            self.servers[s].tabs[t].tab.input.set_focus();
        }
        if let Some(error) = &self.speech.error {
            if !self.speech_error_shown {
                self.speech_error_shown = true;
                self.line(s, t, &format!("Speech isn't available: {error}"));
            }
        }
        if let Some(error) = self.sync_error.take() {
            self.append_output(s, 0, &format!("Synchronization state: {error}"));
        }
        s
    }

    // ---- speech toggles ------------------------------------------------------------------

    /// F5: speech on/off for the selected tab. Says the new state.
    pub fn toggle_tab_speech(&mut self) {
        let Some((s, t)) = self.current_tab() else {
            return;
        };
        let tab = &mut self.servers[s].tabs[t];
        let feedback = match tab.kind {
            TabKind::Server => "The server tab is spoken while it's selected.".to_string(),
            TabKind::Channel | TabKind::Query => {
                tab.speech = !tab.speech;
                let name = if tab.kind == TabKind::Channel {
                    spoken_channel(&tab.name)
                } else {
                    &tab.name
                };
                format!(
                    "Speech {} for {name}",
                    if tab.speech { "on" } else { "off" }
                )
            }
        };
        self.feedback(s, t, &feedback);
    }

    /// Shift+F5: all speech on/off. Says the new state either way.
    pub fn toggle_all_speech(&mut self) {
        self.speech.enabled = !self.speech.enabled;
        let feedback = if self.speech.enabled {
            "All speech on"
        } else {
            "All speech off"
        };
        match self.current_tab() {
            Some((s, t)) => self.feedback(s, t, feedback),
            None => self.speech.say(feedback, true),
        }
    }

    /// Speaks the result of a toggle, cutting off anything queued. Without speech, it's
    /// written to the tab instead so there's still some feedback.
    fn feedback(&mut self, s: usize, t: usize, text: &str) {
        if self.speech.error.is_some() {
            self.show(s, t, Local::now(), text);
        } else {
            self.speech.say(text, true);
        }
    }

    /// A problem with something the user typed: written to the tab and spoken (interrupting,
    /// and even with all speech off, since it answers the user's own action).
    fn complain(&mut self, s: usize, t: usize, text: &str) {
        // Not through `line`, which would speak it a second time in the server tab.
        let now = Local::now();
        self.append_output(s, t, &timestamped(now, text));
        self.log_tab(s, t, now, text);
        self.speech.say(text, true);
    }

    /// The selected tab, as (server, tab) indices.
    fn current_tab(&self) -> Option<(usize, usize)> {
        let selected = self.notebook.selection();
        (selected >= 0)
            .then(|| self.page_to_tab(selected as usize))
            .flatten()
    }

    // ---- tabs ----------------------------------------------------------------------------

    /// Notebook page index of a server's tab.
    fn page_index(&self, s: usize, t: usize) -> usize {
        self.servers[..s]
            .iter()
            .map(|server| server.tabs.len())
            .sum::<usize>()
            + t
    }

    fn find_tab(&self, s: usize, name: &str) -> Option<usize> {
        self.servers[s]
            .tabs
            .iter()
            .position(|tab| tab.kind != TabKind::Server && irc_eq(&tab.name, name))
    }

    fn locate(&self, id: TabId) -> Option<(usize, usize)> {
        self.servers.iter().enumerate().find_map(|(s, server)| {
            server
                .tabs
                .iter()
                .position(|tab| tab.id == id)
                .map(|t| (s, t))
        })
    }

    /// The tab for a channel or nick, created if missing. `select` also selects it (and focuses
    /// its input) — only for tabs the user asked for, never for incoming traffic.
    fn open_tab(&mut self, s: usize, kind: TabKind, name: &str, select: bool) -> usize {
        match self.find_tab(s, name) {
            Some(t) => {
                if select {
                    self.select_tab(s, t);
                }
                t
            }
            None => {
                let t = self.insert_tab(s, kind, name, select);
                if select {
                    self.servers[s].tabs[t].tab.input.set_focus();
                }
                t
            }
        }
    }

    /// Creates a tab in its place: channels after the server's last channel, queries at the end.
    fn insert_tab(&mut self, s: usize, kind: TabKind, name: &str, select: bool) -> usize {
        let tabs = &self.servers[s].tabs;
        let t = match kind {
            TabKind::Server => 0,
            TabKind::Channel => tabs.iter().filter(|tab| tab.kind != TabKind::Query).count(),
            TabKind::Query => tabs.len(),
        };
        let page = self.page_index(s, t);

        let tab = Tab::new(&self.notebook);
        self.notebook
            .insert_page(page, &tab.panel, name, select, None);
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        bind_input(self.me.clone(), id, &tab);

        self.servers[s].tabs.insert(
            t,
            ChatTab {
                id,
                kind,
                name: name.to_string(),
                tab,
                speech: kind != TabKind::Server,
                joined: false,
                rejoin: false,
                key: None,
                nicks: HashSet::new(),
                history: History::default(),
                deduper: Deduper::new(&[]),
                oldest: None,
                history_boundary: None,
                history_attempt: None,
                marker_attempt: None,
                first_chat: None,
            },
        );
        if kind != TabKind::Server {
            let key = self.conversation_key(s, name);
            let read = self
                .sync_data
                .conversations
                .get(&key)
                .and_then(|c| c.read.clone());
            if self.servers[s].ready {
                let _ = self.servers[s].markers.restore(name, read.as_deref());
            }
            if !self.servers[s].cold_start {
                let ids = self
                    .sync_data
                    .conversations
                    .get(&key)
                    .map(|state| state.recent_ids.clone())
                    .unwrap_or_default();
                self.servers[s].tabs[t].deduper = Deduper::new(&ids);
            }
            if kind == TabKind::Query
                && self.servers[s].registered
                && self.servers[s].role != irctest::bouncer::ConnectionRole::Control
            {
                self.request_context(s, t);
            }
        }
        t
    }

    fn select_tab(&self, s: usize, t: usize) {
        self.notebook.set_selection(self.page_index(s, t));
        self.servers[s].tabs[t].tab.input.set_focus();
    }

    fn remove_tab(&mut self, s: usize, t: usize) {
        self.close_log(s, t);
        let page = self.page_index(s, t);
        let removed = self.servers[s].tabs.remove(t);
        self.notebook.remove_page(page);
        removed.tab.panel.destroy();

        // The focused input box was just destroyed; give focus to whatever page is now showing.
        let selected = self.notebook.selection();
        if selected >= 0 {
            if let Some((s, t)) = self.page_to_tab(selected as usize) {
                self.servers[s].tabs[t].tab.input.set_focus();
            }
        }
    }

    /// /close or Ctrl+F4: parts a channel (if we're in it) and removes the tab. Server tabs stay.
    fn close_tab(&mut self, s: usize, t: usize) {
        match self.servers[s].tabs[t].kind {
            TabKind::Server => self.complain(
                s,
                t,
                "The server tab can't be closed. Use /quit to disconnect.",
            ),
            TabKind::Channel => {
                if self.servers[s].tabs[t].joined {
                    if let Some(sender) = self.servers[s].sender.clone() {
                        let _ = net::send_raw(
                            &sender,
                            &format!("PART {}", self.servers[s].tabs[t].name),
                        );
                    }
                }
                self.remove_tab(s, t);
            }
            TabKind::Query => self.remove_tab(s, t),
        }
    }

    /// Ctrl+F4 on the selected tab.
    pub fn close_current(&mut self) {
        if let Some((s, t)) = self.current_tab() {
            self.close_tab(s, t);
        }
    }

    /// A query tab's nick changed: new name and label.
    fn rename_tab(&mut self, s: usize, t: usize, name: &str) {
        let old = self.servers[s].tabs[t].name.clone();
        self.close_log(s, t);
        let old_key = self.conversation_key(s, &old);
        let new_key = self.conversation_key(s, name);
        if old_key != new_key {
            if let Some(state) = self.sync_data.conversations.remove(&old_key) {
                merge_conversation(
                    self.sync_data.conversations.entry(new_key).or_default(),
                    state,
                );
                self.writer.update(self.sync_data.clone());
            }
        }
        let _ = self.servers[s].markers.rename(&old, name);
        let key = self.conversation_key(s, name);
        let read = self
            .sync_data
            .conversations
            .get(&key)
            .and_then(|state| state.read.clone());
        let _ = self.servers[s].markers.restore(name, read.as_deref());
        if let Some(requested) = self.servers[s].marker_pending.remove(&irc_lower(&old)) {
            for destination in self.servers[s].marker_aliases.values_mut() {
                if irc_eq(destination, &old) {
                    *destination = name.into();
                }
            }
            self.servers[s]
                .marker_aliases
                .insert(irc_lower(&old), name.into());
            let pending = self.servers[s]
                .marker_pending
                .entry(irc_lower(name))
                .or_insert(requested.clone());
            if marker_acknowledges(pending, Some(&requested)) {
                *pending = requested;
            }
        }
        if self.servers[s].tabs[t].history_boundary.is_some() {
            for destination in self.servers[s].history_aliases.values_mut() {
                if irc_eq(destination, &old) {
                    *destination = name.into();
                }
            }
            self.servers[s]
                .history_aliases
                .insert(irc_lower(&old), name.into());
        }
        for batch in self.servers[s].playback.batches.values_mut() {
            if let Some(at) = batch.boundaries.remove(&irc_lower(&old)) {
                batch.boundaries.insert(irc_lower(name), at);
            }
            for (target, _) in &mut batch.messages {
                if target.as_deref().is_some_and(|target| irc_eq(target, &old)) {
                    *target = Some(name.into());
                }
            }
        }
        self.servers[s].tabs[t].name = name.into();
        self.update_unread(s, name);
    }

    /// Up/Down in an input box: recall an earlier line. Not spoken here: NVDA reads the new
    /// text itself.
    fn recall(&mut self, id: TabId, up: bool) {
        let Some((s, t)) = self.locate(id) else {
            return;
        };
        let tab = &mut self.servers[s].tabs[t];
        let recalled = if up {
            tab.history.up(&tab.tab.input.get_value())
        } else {
            tab.history.down()
        };
        let Some(text) = recalled else { return };
        tab.tab.input.set_value(&text);
        tab.tab.input.set_insertion_point_end();
    }

    fn page_to_tab(&self, mut page: usize) -> Option<(usize, usize)> {
        for (s, server) in self.servers.iter().enumerate() {
            if page < server.tabs.len() {
                return Some((s, page));
            }
            page -= server.tabs.len();
        }
        None
    }

    /// Appends a line stamped with the current time, and logs it.
    fn line(&mut self, s: usize, t: usize, text: &str) {
        self.line_at(s, t, Local::now(), text);
    }

    fn line_at(&mut self, s: usize, t: usize, time: DateTime<Local>, text: &str) {
        self.show(s, t, time, text);
        self.log_tab(s, t, time, text);
    }

    /// Appends a line without logging it, and speaks it if it's in the server tab the user
    /// is looking at.
    fn show(&mut self, s: usize, t: usize, time: DateTime<Local>, text: &str) {
        self.append_output(s, t, &timestamped(time, text));
        if self.speaks_lines(s, t) {
            self.speech.message(text);
        }
    }

    /// Whether every line shown in tab `t` is spoken: it's a server tab the user is looking at.
    fn speaks_lines(&self, s: usize, t: usize) -> bool {
        self.servers[s].tabs[t].kind == TabKind::Server
            && self.current_tab() == Some((s, t))
            && app_is_foreground()
    }

    /// Writes a line to tab `t`'s log.
    fn log_tab(&mut self, s: usize, t: usize, time: DateTime<Local>, text: &str) {
        let target = log_target(&self.servers[s].tabs[t]).to_string();
        self.log(s, &target, time, text);
    }

    /// Writes a line to `target`'s log, if logging is on. The first failure is shown in the
    /// server tab; later ones are silent.
    fn log(&mut self, s: usize, target: &str, time: DateTime<Local>, text: &str) {
        self.log_with_policy(s, target, time, text, false);
    }
    fn log_with_policy(
        &mut self,
        s: usize,
        target: &str,
        time: DateTime<Local>,
        text: &str,
        quiet: bool,
    ) {
        if !self.config.log {
            return;
        }
        if !self.servers[s].ready && target != SERVER_TARGET {
            let pending = &mut self.servers[s].deferred_logs;
            if pending.len() < 1000
                && pending
                    .iter()
                    .map(|(_, _, text, _)| text.len())
                    .sum::<usize>()
                    + text.len()
                    <= 4 * 1024 * 1024
            {
                pending.push((target.into(), time, text.into(), quiet));
            }
            return;
        }
        if let Err(e) = self
            .logger
            .write(&self.log_namespace(s), target, time, text)
            && !self.log_error_shown
        {
            self.log_error_shown = true;
            let text = format!("Logging failed: {e}");
            if quiet {
                self.append_output(s, 0, &timestamped(Local::now(), &text));
            } else {
                self.show(s, 0, Local::now(), &text);
            }
        }
    }

    /// Closes a tab's log file (the tab is going away or being renamed).
    fn log_namespace(&self, s: usize) -> String {
        match &self.servers[s].actual_network {
            Some(id) => irctest::log::child_namespace(&self.servers[s].account_key.canonical(), id),
            None => self.servers[s].config.name.clone(),
        }
    }

    fn close_log(&mut self, s: usize, t: usize) {
        let target = log_target(&self.servers[s].tabs[t]).to_string();
        self.logger.close(&self.log_namespace(s), &target);
    }

    fn conversation_key(&self, s: usize, target: &str) -> String {
        ConversationKey {
            account: self.servers[s].account_key.clone(),
            network_id: self.servers[s].actual_network.clone(),
            target: target.into(),
        }
        .canonical()
    }
    fn line_count(&self, s: usize, t: usize) -> usize {
        self.servers[s].tabs[t].tab.line_count()
    }
    fn start_history_targets(&mut self, s: usize) {
        if !self.servers[s].ready
            || self.servers[s].role == irctest::bouncer::ConnectionRole::Control
            || self.servers[s].history_targets_started
            || !self.servers[s].capabilities.enabled("draft/chathistory")
        {
            return;
        }
        if let Some(sender) = &self.servers[s].sender {
            if sender.discover_history_targets(None).is_ok() {
                self.servers[s].history_targets_started = true;
            }
        }
    }
    fn request_context(&mut self, s: usize, t: usize) {
        if !self.servers[s].ready
            || !self.servers[s].registered
            || self.servers[s].role == irctest::bouncer::ConnectionRole::Control
        {
            return;
        }
        let attempt = self.servers[s].attempt;
        let target = self.servers[s].tabs[t].name.clone();
        if self.servers[s].tabs[t].kind == TabKind::Query
            && self.servers[s].capabilities.enabled("draft/read-marker")
            && self.servers[s].tabs[t].marker_attempt != Some(attempt)
        {
            if self.servers[s].sender.as_ref().is_some_and(|sender| {
                sender
                    .send(Command::Raw("MARKREAD".into(), vec![target.clone()]))
                    .is_ok()
            }) {
                self.servers[s].tabs[t].marker_attempt = Some(attempt);
            }
        }
        if self.servers[s].tabs[t].history_attempt == Some(attempt)
            || !self.servers[s].capabilities.enabled("draft/chathistory")
        {
            return;
        }
        let key = self.conversation_key(s, &target);
        let received = self
            .sync_data
            .conversations
            .get(&key)
            .and_then(|c| c.received.clone());
        let reason = if received.is_some() {
            HistoryReason::CatchUp
        } else {
            HistoryReason::Initial
        };
        if self.request_history(s, t, reason, 100, received, false) {
            self.servers[s].tabs[t].history_attempt = Some(attempt);
        }
    }
    fn request_history(
        &mut self,
        s: usize,
        t: usize,
        reason: HistoryReason,
        count: usize,
        cursor: Option<Cursor>,
        explicit: bool,
    ) -> bool {
        let error = if self.servers[s].sender.is_none() {
            Some("Not connected.")
        } else if !self.servers[s].ready {
            Some("Wait for server registration to finish.")
        } else if !self.servers[s].capabilities.enabled("draft/chathistory") {
            Some("This server does not support chat history.")
        } else if self.servers[s].tabs[t].history_boundary.is_some()
            || self.servers[s]
                .history_aliases
                .contains_key(&irc_lower(&self.servers[s].tabs[t].name))
        {
            Some("History is already pending for this conversation.")
        } else {
            None
        };
        if let Some(error) = error {
            if explicit {
                self.complain(s, t, error);
            }
            return false;
        }
        let target = self.servers[s].tabs[t].name.clone();
        let boundary = if reason == HistoryReason::Older {
            self.servers[s].tabs[t]
                .first_chat
                .unwrap_or(self.line_count(s, t))
        } else {
            self.line_count(s, t)
        };
        let request = HistoryRequest {
            target,
            reason,
            count,
            cursor,
        };
        if self.servers[s]
            .sender
            .as_ref()
            .is_some_and(|sender| sender.history(request).is_ok())
        {
            self.servers[s].tabs[t].history_boundary = Some(boundary);
            true
        } else {
            if explicit {
                self.complain(s, t, "Couldn't request history: connection closed.");
            }
            false
        }
    }
    fn observe_chat(&mut self, s: usize, t: usize, message: &Message) {
        if !displayed_chat(message) {
            return;
        }
        let target = self.servers[s].tabs[t].name.clone();
        self.servers[s].markers.observe(&target, message);
        if !self.servers[s].ready {
            if self.servers[s].early_chat.len() < 4096 {
                let at = self.line_count(s, t);
                self.servers[s]
                    .early_chat
                    .push((target, message.clone(), at));
            }
            return;
        }
        if let Some(cursor) = cursor(message) {
            if self.servers[s].tabs[t].oldest.as_ref().is_none_or(|old| {
                irctest::sync_state::timestamp(&cursor.time).ok()
                    < irctest::sync_state::timestamp(&old.time).ok()
            }) {
                self.servers[s].tabs[t].oldest = Some(cursor.clone());
            }
            let key = self.conversation_key(s, &target);
            let ids = self.servers[s].tabs[t].deduper.recent_ids(256);
            let state = self.sync_data.conversations.entry(key).or_default();
            state.observe(cursor);
            merge_ids(&mut state.recent_ids, ids);
            self.writer.update(self.sync_data.clone());
        }
        self.update_unread(s, &target);
    }
    fn update_unread(&self, s: usize, target: &str) {
        if let Some(t) = self.find_tab(s, target) {
            let unread = self.servers[s].capabilities.enabled("draft/read-marker")
                && self.servers[s].markers.unread(target);
            let label = format!(
                "{}{}",
                self.servers[s].tabs[t].name,
                if unread { " [unread]" } else { "" }
            );
            set_page_text(&self.notebook, self.page_index(s, t), &label);
        }
    }
    fn restore_history(
        &mut self,
        s: usize,
        target: &str,
        reason: HistoryReason,
        messages: Vec<Message>,
        pending: Option<bool>,
        boundary: Option<usize>,
    ) {
        // Opening an unsolicited query may start its independent requested context;
        // unsolicited delivery must not clear or take over that request's boundary.
        let t = self.open_tab(
            s,
            if target.is_channel_name() {
                TabKind::Channel
            } else {
                TabKind::Query
            },
            target,
            false,
        );
        let boundary = boundary
            .or_else(|| pending.and(self.servers[s].tabs[t].history_boundary))
            .unwrap_or(self.line_count(s, t));
        let policy = delivery_policy(Some(reason), self.servers[s].cold_start);
        self.servers[s].tabs[t].deduper.begin_history();
        let mut lines = Vec::new();
        let mut first_cursor = None;
        for message in messages {
            if !displayed_chat(&message) {
                continue;
            }
            if !self.servers[s].tabs[t].deduper.accept(&message, true) {
                continue;
            }
            let Some(formatted) = format_message(&message) else {
                continue;
            };
            if first_cursor.is_none() {
                first_cursor = cursor(&message);
            }
            let time = message_time(&message);
            lines.push(if let Some(time) = time {
                irctest::format::history_timestamped(time, &formatted.text)
            } else {
                format!("[unknown date] {}", formatted.text)
            });
            if policy.log {
                if let Some(time) = time {
                    self.log_with_policy(s, target, time, &formatted.text, true);
                }
            }
            self.observe_chat(s, t, &message);
        }
        self.servers[s].tabs[t].deduper.end_history();
        update_page_oldest(&mut self.servers[s].tabs[t].oldest, reason, first_cursor);
        let edit = self.servers[s].tabs[t].tab.insert_history(&lines, boundary);
        self.shift_history_positions(s, t, edit.at, edit.inserted, edit.trimmed);
        if !lines.is_empty() {
            let first = edit.at.saturating_sub(edit.trimmed);
            self.servers[s].tabs[t].first_chat = Some(
                self.servers[s].tabs[t]
                    .first_chat
                    .unwrap_or(first)
                    .min(first),
            );
        }
        if let Some(pending) = pending {
            self.servers[s].tabs[t].history_boundary = if pending {
                Some((edit.at + edit.inserted).saturating_sub(edit.trimmed))
            } else {
                None
            };
            if !pending
                && reason == HistoryReason::CatchUp
                && self.servers[s].cold_start
                && self.servers[s].tabs[t].first_chat.is_none()
            {
                self.request_history(s, t, HistoryReason::Initial, 100, None, false);
            }
        }
    }

    // ---- network events ------------------------------------------------------------------

    fn network_update(&mut self, s: usize, update: irctest::bouncer::NetworkUpdate) {
        if !self.servers[s].ready
            || self.servers[s].role != irctest::bouncer::ConnectionRole::Control
            || self.servers[s].user_quit
        {
            return;
        }
        match &update {
            irctest::bouncer::NetworkUpdate::Upsert(network) => {
                self.servers[s].network_ids.insert(network.id.clone());
            }
            irctest::bouncer::NetworkUpdate::Remove(id) => {
                self.servers[s].network_ids.remove(id);
                self.servers[s].manual_suppressed.remove(id);
            }
        }
        let actions = self.servers[s].group.apply(update);
        self.child_actions(s, actions);
        let parent = self.servers[s].id;
        let labels: Vec<_> = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, c)| c.parent == Some(parent))
            .filter_map(|(c, child)| {
                let network = self.servers[s]
                    .group
                    .network(child.network_id.as_deref()?)?;
                Some((
                    c,
                    single_line(&format!(
                        "{} / {} [{}]",
                        self.servers[s].config.name,
                        network.name(),
                        network.id
                    )),
                ))
            })
            .collect();
        for (c, label) in labels {
            self.servers[c].tabs[0].name = label.clone();
            set_page_text(&self.notebook, self.page_index(c, 0), &label);
        }
    }
    fn child_actions(&mut self, parent: usize, actions: Vec<irctest::connections::ChildAction>) {
        use irctest::connections::ChildAction;
        for action in actions {
            let id = match &action {
                ChildAction::Start(id) | ChildAction::Stop(id) => id,
            };
            let parent_id = self.servers[parent].id;
            let existing = self
                .servers
                .iter()
                .position(|c| c.parent == Some(parent_id) && c.network_id.as_ref() == Some(id));
            match action {
                ChildAction::Start(id) => {
                    if self.shutting_down || self.servers[parent].user_quit {
                        continue;
                    }
                    if self.manually_bound(parent, &id) {
                        self.servers[parent].manual_suppressed.insert(id.clone());
                        let actions = self.servers[parent].group.suppress(&id);
                        self.child_actions(parent, actions);
                        continue;
                    }
                    let c = existing.unwrap_or_else(|| {
                        let config = self.servers[parent].config.clone();
                        let c = self.add_server(config, false);
                        self.servers[c].parent = Some(parent_id);
                        self.servers[c].network_id = Some(id.clone());
                        self.servers[c].actual_network = Some(id.clone());
                        c
                    });
                    self.servers[c].config = self.servers[parent].config.clone();
                    self.servers[c].config.discover_networks = false;
                    self.servers[c].accepted_fingerprints =
                        self.servers[parent].accepted_fingerprints.clone();
                    self.servers[c].user_quit = false;
                    self.servers[c].reconnect_timer.stop();
                    self.start(c);
                }
                ChildAction::Stop(_) => {
                    if let Some(c) = existing {
                        self.servers[c].user_quit = true;
                        self.servers[c].reconnect_timer.stop();
                        self.servers[c].attempt += 1;
                        if let Some(task) = self.servers[c].task.take() {
                            task.abort();
                        }
                        self.mark_disconnected(c, "Bouncer network removed or disconnected.");
                    }
                }
            }
        }
    }
    fn manually_bound(&self, parent: usize, id: &str) -> bool {
        let parent = &self.servers[parent];
        let login = bouncer_login_key(&parent.active_config, &parent.config);
        self.servers.iter().any(|server| {
            server.parent.is_none()
                && server.id != parent.id
                && server.ready
                && !server.user_quit
                && server.task.is_some()
                && server.role == irctest::bouncer::ConnectionRole::Bound(id.into())
                && bouncer_login_key(&server.active_config, &server.config) == login
        })
    }
    fn reconcile_manual_bindings(&mut self) {
        let parents: Vec<_> = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.parent.is_none()
                    && s.ready
                    && !s.user_quit
                    && s.role == irctest::bouncer::ConnectionRole::Control
            })
            .map(|(s, _)| s)
            .collect();
        for parent in parents {
            let ids: Vec<_> = self.servers[parent].network_ids.iter().cloned().collect();
            for id in ids {
                let bound = self.manually_bound(parent, &id);
                if bound && self.servers[parent].manual_suppressed.insert(id.clone()) {
                    let actions = self.servers[parent].group.suppress(&id);
                    self.child_actions(parent, actions);
                } else if !bound && self.servers[parent].manual_suppressed.remove(&id) {
                    let actions = self.servers[parent].group.resume(&id);
                    self.child_actions(parent, actions);
                }
            }
        }
    }
    fn child_registration_finished(&mut self, s: usize) {
        if let (Some(parent), Some(id)) =
            (self.servers[s].parent, self.servers[s].network_id.clone())
        {
            if let Some(p) = self.servers.iter().position(|p| p.id == parent) {
                let actions = self.servers[p].group.registration_finished(&id);
                self.child_actions(p, actions);
            }
        }
    }
    fn user_disconnect_group(&mut self, s: usize) {
        if let (Some(parent), Some(id)) =
            (self.servers[s].parent, self.servers[s].network_id.clone())
        {
            if let Some(p) = self.servers.iter().position(|p| p.id == parent) {
                self.servers[p].manual_suppressed.remove(&id);
                let actions = self.servers[p].group.suppress(&id);
                self.child_actions(p, actions);
            }
        } else {
            let actions = self.servers[s].group.disconnect();
            self.child_actions(s, actions);
        }
    }

    fn playback_boundary(&mut self, s: usize, boundary: irctest::playback::BatchBoundary) {
        use irctest::playback::BatchBoundary;
        match boundary {
            BatchBoundary::Start(meta) if meta.history => {
                let boundaries = self.servers[s]
                    .tabs
                    .iter()
                    .filter(|tab| tab.kind != TabKind::Server)
                    .map(|tab| (irc_lower(&tab.name), tab.tab.line_count()))
                    .collect();
                self.servers[s].playback.batches.insert(
                    meta.id,
                    BufferedPlayback {
                        boundaries,
                        ..Default::default()
                    },
                );
            }
            BatchBoundary::End(meta) => {
                if let Some(batch) = self.servers[s].playback.take(&meta.id) {
                    let mut groups: Vec<(String, Vec<Message>)> = Vec::new();
                    let me = self.servers[s].nick.clone();
                    for (target, message) in batch.messages {
                        if !displayed_chat(&message) {
                            continue;
                        }
                        let target = target.or_else(|| match route(&message, &me) {
                            Route::Channel(n) | Route::Query(n) | Route::TabIfOpen(n) => Some(n),
                            _ => None,
                        });
                        let Some(target) = target else {
                            continue;
                        };
                        if let Some((_, messages)) =
                            groups.iter_mut().find(|(name, _)| irc_eq(name, &target))
                        {
                            messages.push(message);
                        } else {
                            groups.push((target, vec![message]));
                        }
                    }
                    for (target, messages) in groups {
                        let boundary = batch.boundaries.get(&irc_lower(&target)).copied();
                        self.restore_history(
                            s,
                            &target,
                            HistoryReason::Initial,
                            messages,
                            None,
                            boundary,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    fn fail_playback(&mut self, s: usize, text: &str) {
        if let Some(task) = self.servers[s].task.take() {
            task.abort();
        }
        self.servers[s].attempt += 1;
        // Historical protocol errors stay quiet even when the server tab is selected.
        self.servers[s].sender = None;
        self.servers[s].ready = false;
        self.servers[s].registered = false;
        self.servers[s].capabilities = Default::default();
        self.servers[s].playback.clear();
        for tab in &mut self.servers[s].tabs {
            tab.history_boundary = None;
            tab.rejoin |= tab.joined;
            tab.joined = false;
            tab.nicks.clear();
        }
        self.append_output(s, 0, &timestamped(Local::now(), text));
        self.child_registration_finished(s);
        if !self.servers[s].user_quit && !self.shutting_down {
            let delay = net::reconnect_delay(self.servers[s].attempts);
            self.servers[s].attempts += 1;
            self.servers[s]
                .reconnect_timer
                .start(delay.as_millis() as i32, true);
        }
    }
    pub fn accepts_event(&self, id: ServerId, attempt: u64) -> bool {
        !self.shutting_down
            && self
                .servers
                .iter()
                .any(|s| s.id == id && s.attempt == attempt)
    }

    pub fn handle_net_event(&mut self, id: ServerId, event: NetEvent) {
        let Some(s) = self.servers.iter().position(|server| server.id == id) else {
            return;
        };
        if self.shutting_down {
            if let NetEvent::UntrustedCertificate(prompt) = event {
                prompt.answer(false);
            }
            return;
        }
        if let Some(error) = self.writer.take_error() {
            self.append_output(
                s,
                0,
                &format!("Could not save synchronization state: {error}"),
            );
        }
        match event {
            NetEvent::NetworkSnapshot(networks) => {
                let present: HashSet<_> = networks.iter().map(|n| n.id.clone()).collect();
                let missing: Vec<_> = self.servers[s]
                    .network_ids
                    .iter()
                    .filter(|n| !present.contains(*n))
                    .cloned()
                    .collect();
                for n in missing {
                    self.network_update(s, irctest::bouncer::NetworkUpdate::Remove(n));
                }
                for n in networks {
                    self.network_update(s, irctest::bouncer::NetworkUpdate::Upsert(n));
                }
            }
            NetEvent::Role(role) => {
                let actual = match &role {
                    irctest::bouncer::ConnectionRole::Bound(id) => Some(id.clone()),
                    _ => None,
                };
                let changed = self.servers[s].actual_network != actual;
                self.servers[s].actual_network = actual;
                self.servers[s].role = role;
                self.servers[s].ready = true;
                if changed {
                    self.reset_conversations(s);
                } else {
                    let targets: Vec<_> = self.servers[s]
                        .tabs
                        .iter()
                        .filter(|t| t.kind != TabKind::Server)
                        .map(|t| t.name.clone())
                        .collect();
                    for target in targets {
                        let key = self.conversation_key(s, &target);
                        let read = self
                            .sync_data
                            .conversations
                            .get(&key)
                            .and_then(|s| s.read.clone());
                        let _ = self.servers[s].markers.restore(&target, read.as_deref());
                        self.update_unread(s, &target);
                    }
                }
                let early = std::mem::take(&mut self.servers[s].early_chat);
                for (target, message, at) in early {
                    if let Some(t) = self.find_tab(s, &target) {
                        if changed {
                            self.servers[s].tabs[t].deduper.accept(&message, false);
                            let first = &mut self.servers[s].tabs[t].first_chat;
                            *first = Some(first.unwrap_or(at).min(at));
                        }
                        self.observe_chat(s, t, &message);
                    }
                }
                let markers = std::mem::take(&mut self.servers[s].early_markers);
                for marker in markers {
                    self.handle_net_event(id, NetEvent::ReadMarker(marker));
                }
                let logs = std::mem::take(&mut self.servers[s].deferred_logs);
                for (target, time, text, quiet) in logs {
                    self.log_with_policy(s, &target, time, &text, quiet);
                }
                self.reconcile_manual_bindings();
                self.refresh_contexts(s);
            }
            NetEvent::Network(update) => self.network_update(s, update),
            NetEvent::Batch(boundary) => self.playback_boundary(s, boundary),
            NetEvent::HistoryMessage {
                batch,
                target,
                message,
            } => {
                if let Err(error) = self.servers[s].playback.push(&batch, target, message) {
                    self.fail_playback(s, &error);
                }
            }
            NetEvent::HistoryComplete {
                target,
                reason,
                messages,
                pending,
                ..
            } => {
                let destination =
                    request_target(&mut self.servers[s].history_aliases, target, !pending);
                self.restore_history(s, &destination, reason, messages, Some(pending), None);
                if !pending {
                    self.refresh_contexts(s);
                }
            }
            NetEvent::HistoryTarget { target, .. } => {
                if !target.is_channel_name() {
                    let t = self.open_tab(s, TabKind::Query, &target, false);
                    self.request_context(s, t);
                }
            }
            NetEvent::HistoryNotice { target, text } => {
                let target = target.map(|target| {
                    request_target(&mut self.servers[s].history_aliases, target, true)
                });
                let t = target
                    .as_deref()
                    .and_then(|n| self.find_tab(s, n))
                    .unwrap_or(0);
                self.servers[s].tabs[t].history_boundary = None;
                self.append_output(s, t, &timestamped(Local::now(), &text));
            }
            NetEvent::ReadMarker(mut update) => {
                if !self.servers[s].ready {
                    if self.servers[s].early_markers.len() < 4096 {
                        self.servers[s].early_markers.push(update);
                    }
                    return;
                }
                update.target =
                    request_target(&mut self.servers[s].marker_aliases, update.target, false);
                let target = update.target.clone();
                let folded = irc_lower(&target);
                let acknowledged =
                    self.servers[s]
                        .marker_pending
                        .get(&folded)
                        .is_some_and(|requested| {
                            marker_acknowledges(requested, update.time.as_deref())
                        });
                match self.servers[s].markers.remote(update) {
                    Ok(changed) => {
                        if changed && self.servers[s].ready {
                            let key = self.conversation_key(s, &target);
                            self.sync_data.conversations.entry(key).or_default().read =
                                self.servers[s].markers.read(&target).map(str::to_owned);
                            self.writer.update(self.sync_data.clone());
                        }
                        self.update_unread(s, &target);
                        if acknowledged {
                            self.servers[s].marker_pending.remove(&folded);
                            retire_target(&mut self.servers[s].marker_aliases, &target);
                            if let Some(t) = self.find_tab(s, &target) {
                                self.line(s, t, "Read marker synchronized.");
                            }
                        }
                    }
                    Err(_) => {}
                }
            }
            NetEvent::ReadMarkerFailed { target, text } => {
                let target = target.map(|target| {
                    request_target(&mut self.servers[s].marker_aliases, target, false)
                });
                let fallback = target
                    .as_deref()
                    .and_then(|target| self.find_tab(s, target))
                    .unwrap_or(0);
                let targets: Vec<_> = if let Some(target) = target {
                    vec![irc_lower(&target)]
                } else {
                    self.servers[s].marker_pending.keys().cloned().collect()
                };
                let mut explicit = false;
                for target in targets {
                    if self.servers[s].marker_pending.remove(&target).is_some() {
                        explicit = true;
                        let t = self.find_tab(s, &target).unwrap_or(0);
                        retire_target(&mut self.servers[s].marker_aliases, &target);
                        self.complain(s, t, &text);
                    }
                }
                if !explicit {
                    self.append_output(s, fallback, &timestamped(Local::now(), &text));
                }
            }
            NetEvent::Capabilities(caps) => {
                let history_lost = self.servers[s].capabilities.enabled("draft/chathistory")
                    && !caps.enabled("draft/chathistory");
                let marker_lost = self.servers[s].capabilities.enabled("draft/read-marker")
                    && !caps.enabled("draft/read-marker");
                if history_lost {
                    self.servers[s].history_targets_started = false;
                    self.servers[s].history_aliases.clear();
                    self.servers[s].playback.clear();
                    for tab in &mut self.servers[s].tabs {
                        tab.history_boundary = None;
                        tab.history_attempt = None;
                    }
                }
                if marker_lost {
                    self.servers[s].marker_pending.clear();
                    self.servers[s].marker_aliases.clear();
                    for tab in &mut self.servers[s].tabs {
                        tab.marker_attempt = None;
                    }
                }
                self.servers[s].capabilities = caps;
                let targets: Vec<_> = self.servers[s]
                    .tabs
                    .iter()
                    .filter(|t| t.kind != TabKind::Server)
                    .map(|t| t.name.clone())
                    .collect();
                for target in targets {
                    self.update_unread(s, &target);
                }
                self.refresh_contexts(s);
            }
            NetEvent::Connected(sender) => {
                self.servers[s].sender = Some(sender);
                self.line(s, 0, "Connected.");
            }
            NetEvent::Message(msg) => self.handle_message(s, &msg),
            NetEvent::NickChanged(nick) => self.servers[s].nick = nick,
            NetEvent::Error(e) => self.line(s, 0, &format!("Error: {e}")),
            // Needs a dialog, which the caller shows: see `certificate_prompt`.
            NetEvent::UntrustedCertificate(prompt) => prompt.answer(false),
            NetEvent::Disconnected { reason, retry } => {
                self.mark_disconnected(s, &format!("Disconnected: {reason}"));
                if retry && !self.servers[s].user_quit {
                    self.schedule_reconnect(s);
                }
            }
        }
    }

    /// A server's certificate isn't trusted: says so in its server tab. Returns the server's
    /// name and saved fingerprint for the dialog, or `None` if the server is gone.
    /// Call `certificate_answered` with the user's choice.
    pub fn certificate_prompt(
        &mut self,
        id: ServerId,
        prompt: &CertificatePrompt,
    ) -> Option<(String, String)> {
        let s = self.servers.iter().position(|server| server.id == id)?;
        let what = if prompt.changed {
            "has changed"
        } else {
            "isn't trusted"
        };
        self.line(
            s,
            0,
            &format!("The server's certificate {what}: {}", prompt.reason),
        );
        self.line(
            s,
            0,
            &format!("SHA-256 fingerprint: {}", prompt.fingerprint),
        );
        let config = &self.servers[s].config;
        Some((config.name.clone(), config.fingerprint.clone()))
    }

    pub fn certificate_answered(
        &mut self,
        id: ServerId,
        prompt: CertificatePrompt,
        choice: CertificateChoice,
    ) {
        let Some(s) = self.servers.iter().position(|server| server.id == id) else {
            return prompt.answer(false);
        };
        let s = self.servers[s]
            .parent
            .and_then(|id| self.servers.iter().position(|p| p.id == id))
            .unwrap_or(s);
        match choice {
            CertificateChoice::TrustOnce => {
                self.servers[s]
                    .accepted_fingerprints
                    .push(prompt.fingerprint.clone());
                self.line(s, 0, "Trusting this certificate until irctest exits.");
            }
            CertificateChoice::TrustAlways => {
                // Trusted now even if saving fails.
                self.servers[s]
                    .accepted_fingerprints
                    .push(prompt.fingerprint.clone());
                self.servers[s].config.fingerprint = prompt.fingerprint.clone();
                let name = self.servers[s].config.name.clone();
                let active = &self.servers[s].config;
                let matching = self.config.servers.iter_mut().find(|config| {
                    irc_eq(&config.name, &name)
                        && config.host.trim().eq_ignore_ascii_case(active.host.trim())
                        && config.port == active.port
                        && config.tls == active.tls
                });
                if let Some(config) = matching {
                    config.fingerprint = prompt.fingerprint.clone();
                } else {
                    self.line(s,0,"The saved endpoint changed; trusting this certificate for the active connection only.");
                    return prompt.answer(true);
                }
                match self.config.save() {
                    Ok(()) => self.line(s, 0, "Certificate fingerprint saved."),
                    Err(e) => self.line(
                        s,
                        0,
                        &format!("Couldn't save the certificate fingerprint: {e}"),
                    ),
                }
            }
            // The Disconnected event that follows says why.
            CertificateChoice::Disconnect => {}
        }
        prompt.answer(choice != CertificateChoice::Disconnect);
    }

    /// The connection is gone: forget it, mark channels parted, and say so in every tab.
    fn mark_disconnected(&mut self, s: usize, text: &str) {
        let server = &mut self.servers[s];
        server.sender = None;
        server.capabilities = Default::default();
        server.task = None;
        server.pending_select.clear();
        server.pending_keys.clear();
        server.registered = false;
        server.ready = false;
        server.playback.clear();
        server.early_chat.clear();
        server.early_markers.clear();
        server.deferred_logs.clear();
        server.marker_pending.clear();
        server.marker_aliases.clear();
        server.history_aliases.clear();
        for tab in &mut server.tabs {
            tab.history_boundary = None;
        }
        for tab in &mut server.tabs {
            // Or-ed: a failed reconnect attempt mustn't forget the channels from before it.
            tab.rejoin |= tab.joined;
            tab.joined = false;
            tab.nicks.clear();
        }
        for t in 0..self.servers[s].tabs.len() {
            self.line(s, t, text);
        }
        self.child_registration_finished(s);
        let names: Vec<_> = self.servers[s]
            .tabs
            .iter()
            .skip(1)
            .map(|t| t.name.clone())
            .collect();
        for name in names {
            self.update_unread(s, &name);
        }
        self.reconcile_manual_bindings();
    }

    fn handle_message(&mut self, s: usize, msg: &Message) {
        let welcome = matches!(msg.command, Command::Response(Response::RPL_WELCOME, _));
        if welcome {
            self.servers[s].attempts = 0;
        }
        let me = self.servers[s].nick.clone();
        if let Some(formatted) = format_message(msg) {
            let time = message_time(msg).unwrap_or_else(Local::now);
            let tabs = self.route_to_tabs(s, msg, &me);
            let chat = displayed_chat(msg);
            if chat {
                if let Some(&t) = tabs
                    .first()
                    .filter(|&&t| self.servers[s].tabs[t].kind != TabKind::Server)
                {
                    if !self.servers[s].tabs[t].deduper.accept(msg, false) {
                        return;
                    }
                    if self.servers[s].tabs[t].first_chat.is_none() {
                        self.servers[s].tabs[t].first_chat = Some(self.line_count(s, t));
                    }
                    self.observe_chat(s, t, msg);
                }
            }
            for &t in &tabs {
                // An outgoing NOTICE without an open conversation is displayed on
                // the server tab, but its log still belongs to the recipient.
                if t == 0
                    && chat
                    && msg
                        .source_nickname()
                        .is_some_and(|source| irc_eq(source, &me))
                {
                    if let Command::NOTICE(target, _) = &msg.command {
                        self.show(s, t, time, &formatted.text);
                        self.log(s, target, time, &formatted.text);
                        continue;
                    }
                }
                self.line_at(s, t, time, &formatted.text);
            }
            if let (Some(speech), Some(&t)) = (&formatted.speech, tabs.first()) {
                self.maybe_speak(s, t, msg, &me, speech);
            }
            if let Some(&t) = tabs.first() {
                self.speak_error(s, t, msg, &formatted.text);
            }
        }
        self.update_membership(s, msg, &me);
        if let Command::JOIN(channel, _, _) = &msg.command {
            if msg.source_nickname().is_some_and(|nick| irc_eq(nick, &me)) {
                if let Some(t) = self.find_tab(s, channel) {
                    self.request_context(s, t);
                }
            }
        }
        if welcome {
            self.servers[s].registered = true;
            self.servers[s].ever_registered = true;
            if self.servers[s].parent.is_none() {
                self.rejoin(s);
            }
            self.child_registration_finished(s);
        }
    }

    /// Speaks a server error reply wherever it's shown: it usually answers something the user
    /// did. Spoken even with all speech off, like `complain`. Nick errors before 001 aren't
    /// spoken: the connection tries the alternative nicks by itself.
    fn speak_error(&mut self, s: usize, t: usize, msg: &Message, text: &str) {
        if !is_error_reply(msg) {
            return;
        }
        let nick_error = matches!(
            msg.command,
            Command::Response(
                Response::ERR_NICKNAMEINUSE | Response::ERR_ERRONEOUSNICKNAME,
                _
            )
        );
        if nick_error && !self.servers[s].registered {
            return;
        }
        // `show` has already spoken it.
        if self.speech.enabled && self.speaks_lines(s, t) {
            return;
        }
        self.speech.say(text, true);
    }

    /// Joins the channels we were in before the connection dropped, with their keys.
    fn rejoin(&mut self, s: usize) {
        let Some(sender) = self.servers[s].sender.clone() else {
            return;
        };
        let mut channels = Vec::new();
        for tab in &mut self.servers[s].tabs {
            if std::mem::take(&mut tab.rejoin) {
                channels.push((tab.name.clone(), tab.key.clone()));
            }
        }
        for line in join_lines(&channels) {
            if let Err(e) = net::send_raw(&sender, &line) {
                self.line(s, 0, &format!("Couldn't rejoin: {e}"));
            }
        }
    }

    /// Notes the keys of a JOIN we're sending, for when the server confirms it.
    fn note_join(&mut self, s: usize, args: &str) {
        for (channel, key) in parse_join(args) {
            if let Some(key) = key {
                self.servers[s]
                    .pending_keys
                    .insert(irc_lower(&channel), key);
            }
        }
    }

    /// Speaks a PRIVMSG shown in tab `t` if the speech settings allow it.
    fn maybe_speak(&mut self, s: usize, t: usize, msg: &Message, me: &str, speech: &str) {
        let Command::PRIVMSG(_, text) = &msg.command else {
            return;
        };
        let body = match parse_ctcp(text) {
            Some(("ACTION", action)) => action,
            _ => text.as_str(),
        };
        // Our own messages are spoken like anyone else's, but saying our own nick isn't a mention.
        let from_me = msg.source_nickname().is_some_and(|n| irc_eq(n, me));
        let mentions_me = !from_me && mentions(&strip_formatting(body), me);
        if should_speak(
            self.speech.enabled,
            self.servers[s].tabs[t].speech,
            mentions_me,
        ) {
            self.speech.message(speech);
        }
    }

    /// The tabs a message should appear in, creating channel/query tabs as needed.
    fn route_to_tabs(&mut self, s: usize, msg: &Message, me: &str) -> Vec<usize> {
        match route(msg, me) {
            Route::Server => vec![0],
            Route::Channel(name) => {
                let ours = matches!(msg.command, Command::JOIN(..))
                    && msg.source_nickname().is_some_and(|n| irc_eq(n, me));
                let select = ours && self.servers[s].pending_select.remove(&irc_lower(&name));
                vec![self.open_tab(s, TabKind::Channel, &name, select)]
            }
            Route::Query(nick) => vec![self.open_tab(s, TabKind::Query, &nick, false)],
            Route::TabIfOpen(name) => vec![self.find_tab(s, &name).unwrap_or(0)],
            Route::UserTabs(nick) if irc_eq(&nick, me) => (0..self.servers[s].tabs.len()).collect(),
            Route::UserTabs(nick) => {
                let key = irc_lower(&nick);
                let tabs: Vec<usize> = self.servers[s]
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, tab)| match tab.kind {
                        TabKind::Channel => tab.nicks.contains(&key),
                        TabKind::Query => irc_eq(&tab.name, &nick),
                        TabKind::Server => false,
                    })
                    .map(|(t, _)| t)
                    .collect();
                tabs
            }
        }
    }

    fn update_membership(&mut self, s: usize, msg: &Message, me: &str) {
        let source = msg.source_nickname().unwrap_or("");
        let is_me = |nick: &str| irc_eq(nick, me);
        match &msg.command {
            Command::JOIN(channel, _, _) => {
                if let Some(t) = self.find_tab(s, channel) {
                    let server = &mut self.servers[s];
                    let tab = &mut server.tabs[t];
                    if is_me(source) {
                        tab.joined = true;
                        tab.rejoin = false;
                        tab.nicks.clear();
                        // Joined without a key (a rejoin, say): keep the one we had.
                        if let Some(key) = server.pending_keys.remove(&irc_lower(channel)) {
                            tab.key = Some(key);
                        }
                    }
                    tab.nicks.insert(irc_lower(source));
                }
            }
            Command::PART(channel, _) => self.left(s, channel, source, is_me(source)),
            Command::KICK(channel, victim, _) => self.left(s, channel, victim, is_me(victim)),
            Command::QUIT(_) => {
                let key = irc_lower(source);
                for tab in &mut self.servers[s].tabs {
                    tab.nicks.remove(&key);
                }
            }
            Command::NICK(new) => {
                // Their query tab follows them, unless the new nick already has one.
                if !is_me(source) {
                    if let (Some(t), None) = (self.find_tab(s, source), self.find_tab(s, new)) {
                        if self.servers[s].tabs[t].kind == TabKind::Query {
                            self.rename_tab(s, t, new);
                        }
                    }
                }
                let (old, new) = (irc_lower(source), irc_lower(new));
                for tab in &mut self.servers[s].tabs {
                    if tab.nicks.remove(&old) {
                        tab.nicks.insert(new.clone());
                    }
                }
            }
            Command::ChannelMODE(channel, modes) => {
                if let Some(t) = self.find_tab(s, channel) {
                    let tab = &mut self.servers[s].tabs[t];
                    for mode in modes {
                        match mode {
                            Mode::Plus(ChannelMode::Key, Some(key)) => tab.key = Some(key.clone()),
                            Mode::Minus(ChannelMode::Key, _) => tab.key = None,
                            _ => {}
                        }
                    }
                }
            }
            // The key we tried was wrong: don't keep it.
            Command::Response(Response::ERR_BADCHANNELKEY, args) if args.len() >= 2 => {
                self.servers[s].pending_keys.remove(&irc_lower(&args[1]));
            }
            Command::Response(Response::RPL_NAMREPLY, args) if args.len() >= 4 => {
                if let Some(t) = self.find_tab(s, &args[2]) {
                    let tab = &mut self.servers[s].tabs[t];
                    for name in args[3].split_whitespace() {
                        // Strip mode prefixes (multi-prefix) and any !user@host (userhost-in-names).
                        let nick = name.trim_start_matches(['~', '&', '@', '%', '+', '!']);
                        let nick = nick.split('!').next().unwrap_or(nick);
                        if !nick.is_empty() {
                            tab.nicks.insert(irc_lower(nick));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Someone left a channel (PART or KICK).
    fn left(&mut self, s: usize, channel: &str, nick: &str, was_me: bool) {
        if let Some(t) = self.find_tab(s, channel) {
            let tab = &mut self.servers[s].tabs[t];
            if was_me {
                tab.joined = false;
                tab.rejoin = false;
                tab.nicks.clear();
            } else {
                tab.nicks.remove(&irc_lower(nick));
            }
        }
    }

    // ---- typed input ---------------------------------------------------------------------

    pub fn handle_input(&mut self, id: TabId, line: &str) {
        let Some((s, t)) = self.locate(id) else {
            return;
        };
        self.servers[s].tabs[t].history.push(line);
        let Some(input) = parse_input(line) else {
            return;
        };
        let kind = self.servers[s].tabs[t].kind;
        let target = self.servers[s].tabs[t].name.clone();

        match input {
            Input::History { count } => {
                if kind == TabKind::Server {
                    return self.complain(
                        s,
                        t,
                        "Use /history in a channel or private conversation.",
                    );
                }
                if self.parted(s, t) {
                    return self.complain(s, t, "Join the channel before requesting history.");
                }
                let r = self.servers[s].tabs[t].oldest.clone();
                self.request_history(s, t, HistoryReason::Older, count, r, true);
            }
            Input::MarkRead => {
                if kind == TabKind::Server {
                    return self.complain(s, t, "Use /markread in a conversation.");
                }
                if !self.servers[s].ready {
                    return self.complain(s, t, "Wait for server registration to finish.");
                }
                if !self.servers[s].capabilities.enabled("draft/read-marker") {
                    return self.complain(s, t, "This server does not support read markers.");
                }
                let Some(sender) = self.sender_or_complain(s, t) else {
                    return;
                };
                match self.servers[s].markers.mark(&target) {
                    Ok(message) => {
                        let requested =
                            self.servers[s].markers.latest(&target).unwrap().to_string();
                        match sender.send(message) {
                            Ok(()) => {
                                self.servers[s]
                                    .marker_pending
                                    .insert(irc_lower(&target), requested);
                                self.line(s, t, "Read marker sent; waiting for the server.");
                            }
                            Err(error) => {
                                self.complain(s, t, &format!("Couldn't send read marker: {error}"))
                            }
                        }
                    }
                    Err(error) => self.complain(s, t, &error),
                }
            }
            Input::Say(text) | Input::Me(text) if kind == TabKind::Server => {
                let _ = text;
                self.complain(s, t, "This is a server tab. Type /command to send a raw IRC command, e.g. /join #channel.");
            }
            Input::Say(_) | Input::Me(_) if self.parted(s, t) => {
                self.complain(
                    s,
                    t,
                    &format!("You're not in {target}. Use /join {target} to rejoin."),
                );
            }
            Input::Say(text) => self.privmsg(s, t, &target, &text, false),
            Input::Me(text) => self.privmsg(s, t, &target, &text, true),
            Input::Msg { target, text } => self.privmsg(s, t, &target, &text, false),
            Input::Join(args) => {
                if let Some(sender) = self.sender_or_complain(s, t) {
                    let channels = args.split_whitespace().next().unwrap_or("");
                    for channel in channels.split(',').filter(|c| !c.is_empty()) {
                        self.servers[s].pending_select.insert(irc_lower(channel));
                    }
                    self.note_join(s, &args);
                    self.send_raw(s, t, &sender, &format!("JOIN {args}"));
                }
            }
            Input::Part { channel, reason } => {
                let channel = match channel {
                    Some(channel) => channel,
                    None if kind == TabKind::Channel => target,
                    None => return self.complain(s, t, "Not in a channel. Use /part #channel."),
                };
                if self.servers[s].sender.is_none() {
                    // Nothing to part, but don't join it again on reconnect.
                    if let Some(c) = self
                        .find_tab(s, &channel)
                        .filter(|&c| self.servers[s].tabs[c].rejoin)
                    {
                        self.servers[s].tabs[c].rejoin = false;
                        return self.complain(
                            s,
                            t,
                            &format!("Not connected. {channel} won't be rejoined."),
                        );
                    }
                }
                if let Some(sender) = self.sender_or_complain(s, t) {
                    let line = match reason {
                        Some(reason) => format!("PART {channel} :{reason}"),
                        None => format!("PART {channel}"),
                    };
                    self.send_raw(s, t, &sender, &line);
                }
            }
            Input::Query { nick, text } => {
                if nick.is_channel_name() {
                    return self.complain(s, t, "That's a channel. Use /join to join it.");
                }
                let q = self.open_tab(s, TabKind::Query, &nick, true);
                if let Some(text) = text {
                    self.privmsg(s, q, &nick, &text, false);
                }
            }
            Input::Nick(nick) => {
                if let Some(sender) = self.sender_or_complain(s, t) {
                    self.send_raw(s, t, &sender, &format!("NICK {nick}"));
                }
            }
            Input::Quit(reason) => {
                if let Some(sender) = self.sender_or_complain(s, t) {
                    self.servers[s].user_quit = true;
                    self.user_disconnect_group(s);
                    if self.servers[s].parent.is_some() {
                        return;
                    }
                    if let Err(e) = sender.send_quit(reason.as_deref().unwrap_or_default()) {
                        self.complain(s, t, &format!("Couldn't send: {e}"));
                    }
                }
            }
            Input::Close => self.close_tab(s, t),
            Input::Raw(line) => {
                if let Some(sender) = self.sender_or_complain(s, t) {
                    let intent = irctest::commands::raw_intent(&line);
                    if matches!(intent, Some(Input::Quit(_))) {
                        self.servers[s].user_quit = true;
                        self.user_disconnect_group(s);
                        if self.servers[s].parent.is_some() {
                            return;
                        }
                    } else if let Some(Input::Join(args)) = intent {
                        self.note_join(s, &args);
                    }
                    self.send_raw(s, t, &sender, &line);
                }
            }
            Input::Usage(text) => self.complain(s, t, text),
        }
    }

    /// Sends a PRIVMSG (or ACTION) and shows it: in the target's tab if one is open,
    /// otherwise in the tab it was typed in.
    fn privmsg(&mut self, s: usize, t: usize, target: &str, text: &str, action: bool) {
        let Some(sender) = self.sender_or_complain(s, t) else {
            return;
        };
        let body = if action {
            format!("\x01ACTION {text}\x01")
        } else {
            text.into()
        };
        let message: Message = Command::PRIVMSG(target.into(), body).into();
        if let Err(error) = sender.send(message.clone()) {
            self.complain(s, t, &format!("Couldn't send: {error}"));
            return;
        }
        self.local_echo(s, message);
    }
    fn local_echo(&mut self, s: usize, mut message: Message) {
        if !irctest::cap::local_echo(&self.servers[s].capabilities, &message.command) {
            return;
        }
        message.prefix = Some(Prefix::Nickname(
            self.servers[s].nick.clone(),
            String::new(),
            String::new(),
        ));
        // Only server-provided timestamps and IDs can advance durable metadata.
        message.tags = None;
        self.handle_message(s, &message);
    }

    fn send_raw(&mut self, s: usize, t: usize, sender: &Sender, line: &str) {
        match net::send_raw(sender, line) {
            Ok(()) => {
                if let Ok(message) = line.parse::<Message>() {
                    if matches!(message.command, Command::PRIVMSG(..) | Command::NOTICE(..)) {
                        self.local_echo(s, message);
                        return;
                    }
                    if matches!(
                        message.command,
                        Command::JOIN(..) | Command::PART(..) | Command::NICK(..)
                    ) {
                        return;
                    }
                }
                self.line(
                    s,
                    t,
                    &format!("-> {}", irctest::format::command_feedback(line)),
                );
            }
            Err(error) => self.complain(s, t, &format!("Couldn't send: {error}")),
        }
    }

    /// A channel tab we're not in while connected (parted, kicked, or not yet rejoined).
    /// While disconnected, "Not connected." says more.
    fn parted(&self, s: usize, t: usize) -> bool {
        let tab = &self.servers[s].tabs[t];
        tab.kind == TabKind::Channel && !tab.joined && self.servers[s].sender.is_some()
    }

    fn sender_or_complain(&mut self, s: usize, t: usize) -> Option<Sender> {
        let sender = self.servers[s].sender.clone();
        if sender.is_none() {
            self.complain(s, t, "Not connected.");
        }
        sender
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Wires a tab's input box: Enter to `State::handle_input`, Up/Down to `State::recall`.
/// The name a tab's log folder is made from.
fn log_target(tab: &ChatTab) -> &str {
    match tab.kind {
        TabKind::Server => SERVER_TARGET,
        TabKind::Channel | TabKind::Query => &tab.name,
    }
}

fn bind_input(state: Weak<RefCell<State>>, id: TabId, tab: &Tab) {
    let input = tab.input;
    let keys_state = state.clone();
    tab.input.on_key_down(move |event| {
        if let WindowEventData::Keyboard(key) = &event {
            let plain = !key.control_down() && !key.shift_down() && !key.alt_down();
            let up = match key.get_key_code() {
                Some(WXK_UP) if plain => Some(true),
                Some(WXK_DOWN) if plain => Some(false),
                _ => None,
            };
            if let (Some(up), Some(state)) = (up, keys_state.upgrade()) {
                event.skip(false);
                state.borrow_mut().recall(id, up);
                return;
            }
        }
        event.skip(true);
    });
    tab.input.on_text_enter(move |event| {
        // Mark Enter as handled; otherwise it reaches the edit control's default handling,
        // which beeps.
        event.skip(false);
        let line = input.get_value();
        input.clear();
        if let Some(state) = state.upgrade() {
            state.borrow_mut().handle_input(id, &line);
        }
    });
}

#[cfg(test)]
mod ircv3_tests {
    use super::*;
    #[test]
    fn unsolicited_batch_keeps_identical_occurrences_in_one_dedup_window() {
        let mut buffers = PlaybackBuffers::default();
        buffers.batches.insert(
            "outer".into(),
            BufferedPlayback {
                boundaries: HashMap::from([("bob".into(), 10)]),
                ..Default::default()
            },
        );
        let message: Message = "@time=2026-10-20T10:00:00Z :bob!u@h PRIVMSG me :same"
            .parse()
            .unwrap();
        buffers.push("outer", None, message.clone()).unwrap();
        buffers.push("outer", None, message.clone()).unwrap();
        buffers.trim("bob", 3);
        let batch = buffers.take("outer").unwrap();
        assert_eq!(batch.boundaries["bob"], 7);
        assert_eq!(batch.messages.len(), 2);
        assert_eq!((buffers.messages, buffers.bytes), (0, 0));
        let mut deduper = Deduper::default();
        deduper.begin_history();
        for (_, message) in &batch.messages {
            assert!(deduper.accept(message, true));
        }
        deduper.end_history();
        deduper.begin_history();
        for (_, message) in &batch.messages {
            assert!(!deduper.accept(message, true));
        }
        deduper.end_history();
    }
    #[test]
    fn unsolicited_playback_limits_apply_across_interleaved_batches() {
        let mut buffers = PlaybackBuffers::default();
        buffers.batches.insert("a".into(), Default::default());
        buffers.batches.insert("b".into(), Default::default());
        let message: Message = Command::PRIVMSG("#test".into(), "x".repeat(6000)).into();
        let mut accepted = 0;
        loop {
            if buffers
                .push(
                    if accepted % 2 == 0 { "a" } else { "b" },
                    None,
                    message.clone(),
                )
                .is_err()
            {
                break;
            }
            accepted += 1;
        }
        assert!(
            accepted < 1000,
            "the shared byte bound must reject before the message-count bound"
        );
        assert!(buffers.bytes <= 4 * 1024 * 1024);
        assert_eq!(buffers.messages, accepted);
        buffers.clear();
        buffers.batches.insert("a".into(), Default::default());
        buffers.batches.insert("b".into(), Default::default());
        let small: Message = Command::PRIVMSG("#test".into(), "small".into()).into();
        for index in 0..1000 {
            buffers
                .push(if index % 2 == 0 { "a" } else { "b" }, None, small.clone())
                .unwrap();
        }
        assert!(buffers.push("a", None, small).is_err());
        assert_eq!(buffers.take("a").unwrap().messages.len(), 500);
        assert_eq!(buffers.messages, 500);
        assert!(buffers.push("missing", None, message).is_err());
        assert_eq!(buffers.messages, 500);
    }
    #[test]
    fn persisted_recent_ids_survive_cold_context_and_remain_bounded() {
        let mut ids = (0..256).map(|id| format!("saved-{id}")).collect();
        merge_ids(&mut ids, ["new-live".into(), "saved-255".into()]);
        assert_eq!(ids.len(), 256);
        assert_eq!(ids.last().map(String::as_str), Some("saved-255"));
        assert!(ids.iter().any(|id| id == "new-live"));
        assert!(ids.iter().any(|id| id == "saved-1"));
        assert!(!ids.iter().any(|id| id == "saved-0"));
    }
    #[test]
    fn successive_older_pages_with_equal_times_advance_opaque_ids() {
        let mut oldest = None;
        for (reason, id) in [
            (HistoryReason::Initial, "201"),
            (HistoryReason::Older, "101"),
            (HistoryReason::Older, "1"),
        ] {
            update_page_oldest(
                &mut oldest,
                reason,
                Some(Cursor {
                    time: "2026-10-20T10:00:00Z".into(),
                    msgid: Some(id.into()),
                }),
            );
            assert_eq!(oldest.as_ref().unwrap().msgid.as_deref(), Some(id));
        }
    }
    #[test]
    fn confirmed_bouncer_login_normalizes_network_and_client_selectors() {
        let config = Config {
            username: "account/network@client".into(),
            ..Default::default()
        };
        let parent = ServerConfig {
            sasl_username: "account@control".into(),
            sasl_password: "one".into(),
            ..Default::default()
        };
        let manual = ServerConfig {
            sasl_username: "account/network@desktop".into(),
            sasl_password: "two".into(),
            ..Default::default()
        };
        assert_eq!(
            bouncer_login_key(&config, &parent),
            bouncer_login_key(&config, &manual)
        );
        let pass_only = ServerConfig::default();
        assert_eq!(
            bouncer_login_key(&config, &parent),
            bouncer_login_key(&config, &pass_only)
        );
        let other = ServerConfig {
            sasl_username: "other/network@desktop".into(),
            ..manual
        };
        assert_ne!(
            bouncer_login_key(&config, &parent),
            bouncer_login_key(&config, &other)
        );
        assert_ne!(
            AccountKey::from_config(&config, &parent),
            AccountKey::from_config(&config, &pass_only)
        );
    }
    #[test]
    fn renamed_request_targets_resolve_for_pages_timeouts_marker_ack_and_failure() {
        let aliases = HashMap::from([("bob".into(), "robert".into())]);
        let mut history = aliases.clone();
        assert_eq!(request_target(&mut history, "BOB".into(), false), "robert");
        assert!(
            history.contains_key("bob"),
            "continuing pages retain their route"
        );
        assert_eq!(request_target(&mut history, "bob".into(), true), "robert");
        assert!(
            history.is_empty(),
            "completion or timeout must retire the route"
        );
        assert_eq!(request_target(&mut history, "bob".into(), false), "bob");
        let mut marker = aliases.clone();
        let target = request_target(&mut marker, "bob".into(), false);
        assert_eq!(target, "robert");
        assert!(marker_acknowledges(
            "2026-10-20T10:00:00Z",
            Some("2026-10-20T10:00:00Z")
        ));
        retire_target(&mut marker, &target);
        assert!(marker.is_empty());
        let mut failure = aliases;
        assert_eq!(request_target(&mut failure, "bob".into(), true), "robert");
        assert!(failure.is_empty());
    }
    #[test]
    fn marker_ack_requires_known_matching_or_later_instant() {
        let requested = "2026-10-20T10:00:00Z";
        assert!(!marker_acknowledges(requested, None));
        assert!(!marker_acknowledges(requested, Some("bad")));
        assert!(!marker_acknowledges(
            requested,
            Some("2026-10-20T09:59:59Z")
        ));
        assert!(marker_acknowledges(
            requested,
            Some("2026-10-20T11:00:00+01:00")
        ));
        assert!(marker_acknowledges(requested, Some("2026-10-20T10:00:01Z")));
    }
    #[test]
    fn authentication_matching_ignores_labels_and_sasl_user_field_network_suffix() {
        let config = Config {
            username: "account/network".into(),
            ..Default::default()
        };
        let a = ServerConfig {
            name: "manual".into(),
            host: "BOUNCER.test".into(),
            sasl_username: "account".into(),
            sasl_password: "one".into(),
            ..Default::default()
        };
        let b = ServerConfig {
            name: "discovery".into(),
            host: "bouncer.test".into(),
            sasl_password: "two".into(),
            ..a.clone()
        };
        let other_user = Config {
            username: "account".into(),
            ..config.clone()
        };
        assert_eq!(login_key(&config, &a), login_key(&other_user, &b));
        let wrong = ServerConfig {
            sasl_username: "other".into(),
            ..b.clone()
        };
        assert_ne!(login_key(&config, &a), login_key(&other_user, &wrong));
    }
    #[test]
    fn read_positions_only_follow_displayable_chat() {
        assert!(displayed_chat(&"PRIVMSG #test :hello".parse().unwrap()));
        assert!(displayed_chat(&"NOTICE #test :hello".parse().unwrap()));
        assert!(displayed_chat(
            &"PRIVMSG #test :\x01ACTION waves\x01".parse().unwrap()
        ));
        assert!(!displayed_chat(
            &"PRIVMSG #test :\x01PING 123\x01".parse().unwrap()
        ));
        assert!(!displayed_chat(
            &"NOTICE #test :\x01PING 123\x01".parse().unwrap()
        ));
    }
    #[test]
    fn trimming_keeps_pending_history_and_first_chat_anchored() {
        let mut position = Some(40);
        trim_anchor(&mut position, 3);
        assert_eq!(position, Some(37));
        trim_anchor(&mut position, 100);
        assert_eq!(position, Some(0));
        let mut missing = None;
        trim_anchor(&mut missing, 3);
        assert!(missing.is_none());
        assert_eq!(single_line("Home\r\n😀\0Network"), "Home😀Network");
    }
    #[test]
    fn merged_conversation_keeps_newer_positions_and_bounded_ids() {
        let mut old = irctest::sync_state::ConversationState::default();
        old.mark_read("2026-10-20T11:00:00Z").unwrap();
        old.observe(Cursor {
            time: "2026-10-20T12:00:00Z".into(),
            msgid: Some("old-id".into()),
        });
        old.recent_ids = vec!["old-id".into()];
        let mut new = irctest::sync_state::ConversationState::default();
        new.mark_read("2026-10-20T10:00:00Z").unwrap();
        new.recent_ids = vec!["new-id".into()];
        merge_conversation(&mut new, old);
        assert_eq!(new.read.as_deref(), Some("2026-10-20T11:00:00Z"));
        assert_eq!(new.received.unwrap().msgid.as_deref(), Some("old-id"));
        assert_eq!(new.recent_ids, vec!["new-id", "old-id"]);
    }
}
