//! Application state on the GUI thread: servers, their tabs, routing and typed commands.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use chrono::{DateTime, Local};
use irc_proto::{ChannelExt, ChannelMode, Command, Message, Mode, Prefix, Response};
use irctest::commands::{Input, join_lines, parse_input, parse_join};
use irctest::config::{Config, ServerConfig};
use irctest::format::{
    format_message, is_error_reply, message_time, parse_ctcp, spoken_channel, strip_formatting, timestamped,
};
use irctest::history::History;
use irctest::log::{Logger, SERVER_TARGET};
use irctest::net::{self, CertificatePrompt, EventSink, NetEvent, Sender, ServerId};
use irctest::route::{Route, irc_eq, irc_lower, route};
use irctest::speech::{Speech, mentions, should_speak};
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
}

pub struct Server {
    id: ServerId,
    config: ServerConfig,
    /// Set once the connection is up; cleared when it drops.
    sender: Option<Sender>,
    /// Set while connecting or connected.
    task: Option<AbortHandle>,
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

impl State {
    pub fn new(config: Config, runtime: Handle, notebook: Notebook, sink: EventSink, placeholder: Panel) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|me| {
            RefCell::new(State {
                config,
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
        let servers: Vec<ServerConfig> = self.config.servers.iter().filter(|s| s.auto_connect).cloned().collect();
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
        let s = match self.servers.iter().position(|s| irc_eq(&s.config.name, &server_config.name)) {
            Some(s) => {
                self.select_tab(s, 0);
                s
            }
            None => self.add_server(server_config.clone()),
        };
        if self.servers[s].task.is_some() {
            self.line(s, 0, "Already connected.");
            return;
        }
        self.servers[s].config = server_config;
        self.reset_reconnect(s);
        self.start(s);
    }

    /// Opens the connection with the server's current settings, unless one is already open.
    fn start(&mut self, s: usize) {
        let server = &mut self.servers[s];
        if server.task.is_some() {
            return;
        }
        let nick = self.config.effective_nick(&server.config).to_string();
        let text = format!("Connecting to {}:{} as {nick}...", server.config.host, server.config.port);
        server.task = Some(net::connect(
            &self.runtime,
            server.id,
            &self.config,
            &server.config,
            &server.accepted_fingerprints,
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
        let name = &self.servers[s].config.name;
        if let Some(config) = self.config.servers.iter().find(|c| irc_eq(&c.name, name)).cloned() {
            self.servers[s].config = config;
        }
    }

    /// Server → Reconnect: drop the selected tab's server connection (if any) and connect again
    /// now, with the latest settings.
    pub fn reconnect_current(&mut self) -> Result<(), String> {
        let (s, _) = self.current_tab().ok_or("There's no server to reconnect.")?;
        self.reset_reconnect(s);
        if let Some(task) = self.servers[s].task.take() {
            // Killed outright, so no Disconnected event will come.
            task.abort();
            self.mark_disconnected(s, "Disconnected: reconnecting.");
        }
        self.refresh_server_config(s);
        self.start(s);
        Ok(())
    }

    /// Waits before reconnecting: 5 seconds, doubling each attempt, at most 5 minutes.
    fn schedule_reconnect(&mut self, s: usize) {
        let server = &mut self.servers[s];
        let delay = net::reconnect_delay(server.attempts);
        server.attempts += 1;
        server.reconnect_timer.start(delay.as_millis() as i32, true);
        self.line(s, 0, &format!("Reconnecting in {} seconds...", delay.as_secs()));
    }

    /// The reconnect timer fired.
    fn reconnect_due(&mut self, id: ServerId) {
        let Some(s) = self.servers.iter().position(|server| server.id == id) else { return };
        if self.servers[s].user_quit || self.servers[s].task.is_some() {
            return;
        }
        self.refresh_server_config(s);
        self.start(s);
    }

    /// Disconnects the server of the selected tab: QUIT if connected, cancels a connect still
    /// in progress, or cancels a pending reconnect. Errors are messages for the user.
    pub fn disconnect_current(&mut self) -> Result<(), String> {
        let (s, _) = self.current_tab().ok_or("There's no server to disconnect.")?;
        let server = &mut self.servers[s];
        server.user_quit = true;
        if server.task.is_none() && server.reconnect_timer.is_running() {
            server.reconnect_timer.stop();
            self.line(s, 0, "Reconnect cancelled.");
            return Ok(());
        }
        if let Some(sender) = &server.sender {
            sender.send_quit("").map_err(|e| format!("Couldn't send QUIT: {e}"))?;
            self.line(s, 0, "Disconnecting...");
            Ok(())
        } else if let Some(task) = server.task.take() {
            // Still connecting: the task is killed, so no Disconnected event will come.
            task.abort();
            self.mark_disconnected(s, "Connection attempt cancelled.");
            Ok(())
        } else {
            Err(format!("{} isn't connected.", server.config.name))
        }
    }

    fn add_server(&mut self, config: ServerConfig) -> usize {
        if let Some(placeholder) = self.placeholder.take() {
            self.notebook.remove_page(0);
            placeholder.destroy();
        }
        let id = self.next_server_id;
        self.next_server_id += 1;
        let label = if config.name.is_empty() { config.host.clone() } else { config.name.clone() };
        let reconnect_timer = Timer::new(&self.notebook);
        let me = self.me.clone();
        reconnect_timer.on_tick(move |_| {
            if let Some(state) = me.upgrade() {
                state.borrow_mut().reconnect_due(id);
            }
        });
        self.servers.push(Server {
            id,
            config,
            sender: None,
            task: None,
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
        let t = self.insert_tab(s, TabKind::Server, &label, true);
        self.servers[s].tabs[t].tab.input.set_focus();
        if let Some(error) = &self.speech.error {
            if !self.speech_error_shown {
                self.speech_error_shown = true;
                self.line(s, t, &format!("Speech isn't available: {error}"));
            }
        }
        s
    }

    // ---- speech toggles ------------------------------------------------------------------

    /// F5: speech on/off for the selected tab. Says the new state.
    pub fn toggle_tab_speech(&mut self) {
        let Some((s, t)) = self.current_tab() else { return };
        let tab = &mut self.servers[s].tabs[t];
        let feedback = match tab.kind {
            TabKind::Server => "The server tab is spoken while it's selected.".to_string(),
            TabKind::Channel | TabKind::Query => {
                tab.speech = !tab.speech;
                let name = if tab.kind == TabKind::Channel { spoken_channel(&tab.name) } else { &tab.name };
                format!("Speech {} for {name}", if tab.speech { "on" } else { "off" })
            }
        };
        self.feedback(s, t, &feedback);
    }

    /// Shift+F5: all speech on/off. Says the new state either way.
    pub fn toggle_all_speech(&mut self) {
        self.speech.enabled = !self.speech.enabled;
        let feedback = if self.speech.enabled { "All speech on" } else { "All speech off" };
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
        self.servers[s].tabs[t].tab.append_line(&timestamped(now, text));
        self.log_tab(s, t, now, text);
        self.speech.say(text, true);
    }

    /// The selected tab, as (server, tab) indices.
    fn current_tab(&self) -> Option<(usize, usize)> {
        let selected = self.notebook.selection();
        (selected >= 0).then(|| self.page_to_tab(selected as usize)).flatten()
    }

    // ---- tabs ----------------------------------------------------------------------------

    /// Notebook page index of a server's tab.
    fn page_index(&self, s: usize, t: usize) -> usize {
        self.servers[..s].iter().map(|server| server.tabs.len()).sum::<usize>() + t
    }

    fn find_tab(&self, s: usize, name: &str) -> Option<usize> {
        self.servers[s].tabs.iter().position(|tab| tab.kind != TabKind::Server && irc_eq(&tab.name, name))
    }

    fn locate(&self, id: TabId) -> Option<(usize, usize)> {
        self.servers.iter().enumerate().find_map(|(s, server)| {
            server.tabs.iter().position(|tab| tab.id == id).map(|t| (s, t))
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
        self.notebook.insert_page(page, &tab.panel, name, select, None);
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
            },
        );
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
            TabKind::Server => self.complain(s, t, "The server tab can't be closed. Use /quit to disconnect."),
            TabKind::Channel => {
                if self.servers[s].tabs[t].joined {
                    if let Some(sender) = self.servers[s].sender.clone() {
                        let _ = net::send_raw(&sender, &format!("PART {}", self.servers[s].tabs[t].name));
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
        self.close_log(s, t);
        self.servers[s].tabs[t].name = name.to_string();
        set_page_text(&self.notebook, self.page_index(s, t), name);
    }

    /// Up/Down in an input box: recall an earlier line. Not spoken here: NVDA reads the new
    /// text itself.
    fn recall(&mut self, id: TabId, up: bool) {
        let Some((s, t)) = self.locate(id) else { return };
        let tab = &mut self.servers[s].tabs[t];
        let recalled = if up { tab.history.up(&tab.tab.input.get_value()) } else { tab.history.down() };
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
        self.servers[s].tabs[t].tab.append_line(&timestamped(time, text));
        if self.speaks_lines(s, t) {
            self.speech.message(text);
        }
    }

    /// Whether every line shown in tab `t` is spoken: it's a server tab the user is looking at.
    fn speaks_lines(&self, s: usize, t: usize) -> bool {
        self.servers[s].tabs[t].kind == TabKind::Server && self.current_tab() == Some((s, t)) && app_is_foreground()
    }

    /// Writes a line to tab `t`'s log.
    fn log_tab(&mut self, s: usize, t: usize, time: DateTime<Local>, text: &str) {
        let target = log_target(&self.servers[s].tabs[t]).to_string();
        self.log(s, &target, time, text);
    }

    /// Writes a line to `target`'s log, if logging is on. The first failure is shown in the
    /// server tab; later ones are silent.
    fn log(&mut self, s: usize, target: &str, time: DateTime<Local>, text: &str) {
        if !self.config.log {
            return;
        }
        let server = &self.servers[s].config.name;
        if let Err(e) = self.logger.write(server, target, time, text)
            && !self.log_error_shown
        {
            self.log_error_shown = true;
            self.show(s, 0, Local::now(), &format!("Logging failed: {e}"));
        }
    }

    /// Closes a tab's log file (the tab is going away or being renamed).
    fn close_log(&mut self, s: usize, t: usize) {
        let target = log_target(&self.servers[s].tabs[t]).to_string();
        self.logger.close(&self.servers[s].config.name, &target);
    }

    // ---- network events ------------------------------------------------------------------

    pub fn handle_net_event(&mut self, id: ServerId, event: NetEvent) {
        let Some(s) = self.servers.iter().position(|server| server.id == id) else { return };
        match event {
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
    pub fn certificate_prompt(&mut self, id: ServerId, prompt: &CertificatePrompt) -> Option<(String, String)> {
        let s = self.servers.iter().position(|server| server.id == id)?;
        let what = if prompt.changed { "has changed" } else { "isn't trusted" };
        self.line(s, 0, &format!("The server's certificate {what}: {}", prompt.reason));
        self.line(s, 0, &format!("SHA-256 fingerprint: {}", prompt.fingerprint));
        let config = &self.servers[s].config;
        Some((config.name.clone(), config.fingerprint.clone()))
    }

    pub fn certificate_answered(&mut self, id: ServerId, prompt: CertificatePrompt, choice: CertificateChoice) {
        let Some(s) = self.servers.iter().position(|server| server.id == id) else {
            return prompt.answer(false);
        };
        match choice {
            CertificateChoice::TrustOnce => {
                self.servers[s].accepted_fingerprints.push(prompt.fingerprint.clone());
                self.line(s, 0, "Trusting this certificate until irctest exits.");
            }
            CertificateChoice::TrustAlways => {
                // Trusted now even if saving fails.
                self.servers[s].accepted_fingerprints.push(prompt.fingerprint.clone());
                self.servers[s].config.fingerprint = prompt.fingerprint.clone();
                let name = self.servers[s].config.name.clone();
                if let Some(config) = self.config.servers.iter_mut().find(|c| irc_eq(&c.name, &name)) {
                    config.fingerprint = prompt.fingerprint.clone();
                }
                match self.config.save() {
                    Ok(()) => self.line(s, 0, "Certificate fingerprint saved."),
                    Err(e) => self.line(s, 0, &format!("Couldn't save the certificate fingerprint: {e}")),
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
        server.task = None;
        server.pending_select.clear();
        server.pending_keys.clear();
        server.registered = false;
        for tab in &mut server.tabs {
            // Or-ed: a failed reconnect attempt mustn't forget the channels from before it.
            tab.rejoin |= tab.joined;
            tab.joined = false;
            tab.nicks.clear();
        }
        for t in 0..self.servers[s].tabs.len() {
            self.line(s, t, text);
        }
    }

    fn handle_message(&mut self, s: usize, msg: &Message) {
        let welcome = matches!(msg.command, Command::Response(Response::RPL_WELCOME, _));
        if welcome {
            // Registered: the next drop starts the backoff from the beginning.
            self.servers[s].attempts = 0;
        }
        let me = self.servers[s].nick.clone();
        if let Some(formatted) = format_message(msg) {
            let time = message_time(msg).unwrap_or_else(Local::now);
            let tabs = self.route_to_tabs(s, msg, &me);
            for &t in &tabs {
                self.line_at(s, t, time, &formatted.text);
            }
            if let (Some(speech), Some(&t)) = (&formatted.speech, tabs.first()) {
                self.maybe_speak(s, t, msg, &me, speech);
            }
            if let Some(&t) = tabs.first() {
                self.speak_error(s, t, msg, &formatted.text);
            }
        }
        // After display: QUIT and NICK are routed by who was in each channel beforehand.
        self.update_membership(s, msg, &me);
        if welcome {
            self.servers[s].registered = true;
            self.rejoin(s);
        }
    }

    /// Speaks a server error reply wherever it's shown: it usually answers something the user
    /// did. Spoken even with all speech off, like `complain`. Nick errors before 001 aren't
    /// spoken: the connection tries the alternative nicks by itself.
    fn speak_error(&mut self, s: usize, t: usize, msg: &Message, text: &str) {
        if !is_error_reply(msg) {
            return;
        }
        let nick_error =
            matches!(msg.command, Command::Response(Response::ERR_NICKNAMEINUSE | Response::ERR_ERRONEOUSNICKNAME, _));
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
        let Some(sender) = self.servers[s].sender.clone() else { return };
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
                self.servers[s].pending_keys.insert(irc_lower(&channel), key);
            }
        }
    }

    /// Speaks a PRIVMSG shown in tab `t` if the speech settings allow it.
    fn maybe_speak(&mut self, s: usize, t: usize, msg: &Message, me: &str, speech: &str) {
        let Command::PRIVMSG(_, text) = &msg.command else { return };
        let body = match parse_ctcp(text) {
            Some(("ACTION", action)) => action,
            _ => text.as_str(),
        };
        // Our own messages are spoken like anyone else's, but saying our own nick isn't a mention.
        let from_me = msg.source_nickname().is_some_and(|n| irc_eq(n, me));
        let mentions_me = !from_me && mentions(&strip_formatting(body), me);
        if should_speak(self.speech.enabled, self.servers[s].tabs[t].speech, mentions_me) {
            self.speech.message(speech);
        }
    }

    /// The tabs a message should appear in, creating channel/query tabs as needed.
    fn route_to_tabs(&mut self, s: usize, msg: &Message, me: &str) -> Vec<usize> {
        match route(msg, me) {
            Route::Server => vec![0],
            Route::Channel(name) => {
                let ours = matches!(msg.command, Command::JOIN(..)) && msg.source_nickname().is_some_and(|n| irc_eq(n, me));
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
        let Some((s, t)) = self.locate(id) else { return };
        self.servers[s].tabs[t].history.push(line);
        let Some(input) = parse_input(line) else { return };
        let kind = self.servers[s].tabs[t].kind;
        let target = self.servers[s].tabs[t].name.clone();

        match input {
            Input::Say(text) | Input::Me(text) if kind == TabKind::Server => {
                let _ = text;
                self.complain(s, t, "This is a server tab. Type /command to send a raw IRC command, e.g. /join #channel.");
            }
            Input::Say(_) | Input::Me(_) if self.parted(s, t) => {
                self.complain(s, t, &format!("You're not in {target}. Use /join {target} to rejoin."));
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
                    if let Some(c) = self.find_tab(s, &channel).filter(|&c| self.servers[s].tabs[c].rejoin) {
                        self.servers[s].tabs[c].rejoin = false;
                        return self.complain(s, t, &format!("Not connected. {channel} won't be rejoined."));
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
                    if let Err(e) = sender.send_quit(reason.as_deref().unwrap_or_default()) {
                        self.complain(s, t, &format!("Couldn't send: {e}"));
                    }
                }
            }
            Input::Close => self.close_tab(s, t),
            Input::Raw(line) => {
                if let Some(sender) = self.sender_or_complain(s, t) {
                    let (command, args) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
                    if command.eq_ignore_ascii_case("QUIT") {
                        self.servers[s].user_quit = true;
                    } else if command.eq_ignore_ascii_case("JOIN") {
                        self.note_join(s, args);
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
        let Some(sender) = self.sender_or_complain(s, t) else { return };
        let result = if action { sender.send_action(target, text) } else { sender.send_privmsg(target, text) };
        if let Err(e) = result {
            return self.complain(s, t, &format!("Couldn't send: {e}"));
        }

        let me = self.servers[s].nick.clone();
        let body = if action { format!("\x01ACTION {text}\x01") } else { text.to_string() };
        let echo = Message {
            tags: None,
            prefix: Some(Prefix::Nickname(me, String::new(), String::new())),
            command: Command::PRIVMSG(target.to_string(), body),
        };
        let Some(formatted) = format_message(&echo) else { return };
        let dest = match self.find_tab(s, target) {
            Some(dest) => {
                self.line(s, dest, &formatted.text);
                dest
            }
            None => {
                // Shown where it was typed, but logged with the rest of that conversation.
                let now = Local::now();
                self.show(s, t, now, &format!("-> {target}: {text}"));
                self.log(s, target, now, &formatted.text);
                t
            }
        };
        if let Some(speech) = &formatted.speech {
            let me = self.servers[s].nick.clone();
            self.maybe_speak(s, dest, &echo, &me, speech);
        }
    }

    fn send_raw(&mut self, s: usize, t: usize, sender: &Sender, line: &str) {
        match net::send_raw(sender, line) {
            Ok(()) => {
                // Commands with their own visible result (JOIN, PART, NICK) don't need an echo.
                let quiet = ["JOIN ", "PART ", "NICK "].iter().any(|c| line.to_ascii_uppercase().starts_with(c));
                if !quiet {
                    self.line(s, t, &format!("-> {}", line.trim()));
                }
            }
            Err(e) => self.complain(s, t, &format!("Couldn't send: {e}")),
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
