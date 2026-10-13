//! Application state on the GUI thread: servers, their tabs, routing and typed commands.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use chrono::{DateTime, Local};
use irc::client::Sender;
use irc::proto::{ChannelExt, Command, Message, Prefix, Response};
use irctest::commands::{Input, parse_input};
use irctest::config::{Config, ServerConfig};
use irctest::format::{format_message, message_time, timestamped};
use irctest::net::{self, EventSink, NetEvent, ServerId};
use irctest::route::{Route, irc_eq, irc_lower, route};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;
use wxdragon::prelude::*;

use super::tab::Tab;

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
    #[allow(dead_code)] // read by speech in step 7
    speech: bool,
    /// Channels: whether we're currently in it.
    joined: bool,
    /// Channels: who's in it, as `irc_lower` nicks. Used to route QUIT and NICK.
    nicks: HashSet<String>,
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
                me: me.clone(),
            })
        })
    }

    /// Temporary until the server manager (step 6): connect to the first configured server.
    pub fn connect_first(&mut self) -> Result<(), String> {
        let server_config = self.config.servers.first().cloned().ok_or("There are no servers in irctest.toml.")?;

        // Reuse the server's tabs if it has them.
        let s = match self.servers.iter().position(|s| s.config.name == server_config.name) {
            Some(s) => s,
            None => self.add_server(server_config),
        };

        let server = &mut self.servers[s];
        if server.task.is_some() {
            self.line(s, 0, "Already connected.");
        } else {
            let nick = self.config.effective_nick(&server.config).to_string();
            let text = format!("Connecting to {}:{} as {nick}...", server.config.host, server.config.port);
            server.task = Some(net::connect(&self.runtime, server.id, &self.config, &server.config, self.sink.clone()));
            self.line(s, 0, &text);
        }
        Ok(())
    }

    fn add_server(&mut self, config: ServerConfig) -> usize {
        if let Some(placeholder) = self.placeholder.take() {
            self.notebook.remove_page(0);
            placeholder.destroy();
        }
        let id = self.next_server_id;
        self.next_server_id += 1;
        let label = if config.name.is_empty() { config.host.clone() } else { config.name.clone() };
        self.servers.push(Server {
            id,
            config,
            sender: None,
            task: None,
            nick: String::new(),
            tabs: Vec::new(),
            pending_select: HashSet::new(),
        });
        let s = self.servers.len() - 1;
        let t = self.insert_tab(s, TabKind::Server, &label, true);
        self.servers[s].tabs[t].tab.input.set_focus();
        s
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
                nicks: HashSet::new(),
            },
        );
        t
    }

    fn select_tab(&self, s: usize, t: usize) {
        self.notebook.set_selection(self.page_index(s, t));
        self.servers[s].tabs[t].tab.input.set_focus();
    }

    fn remove_tab(&mut self, s: usize, t: usize) {
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

    fn page_to_tab(&self, mut page: usize) -> Option<(usize, usize)> {
        for (s, server) in self.servers.iter().enumerate() {
            if page < server.tabs.len() {
                return Some((s, page));
            }
            page -= server.tabs.len();
        }
        None
    }

    /// Appends a line stamped with the current time.
    fn line(&self, s: usize, t: usize, text: &str) {
        self.line_at(s, t, Local::now(), text);
    }

    fn line_at(&self, s: usize, t: usize, time: DateTime<Local>, text: &str) {
        self.servers[s].tabs[t].tab.append_line(&timestamped(time, text));
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
            NetEvent::Disconnected(reason) => {
                let server = &mut self.servers[s];
                server.sender = None;
                server.task = None;
                server.pending_select.clear();
                for tab in &mut server.tabs {
                    tab.joined = false;
                    tab.nicks.clear();
                }
                let text = format!("Disconnected: {reason}");
                for t in 0..self.servers[s].tabs.len() {
                    self.line(s, t, &text);
                }
            }
        }
    }

    fn handle_message(&mut self, s: usize, msg: &Message) {
        let me = self.servers[s].nick.clone();
        if let Some(formatted) = format_message(msg) {
            let time = message_time(msg).unwrap_or_else(Local::now);
            for t in self.route_to_tabs(s, msg, &me) {
                self.line_at(s, t, time, &formatted.text);
            }
        }
        // After display: QUIT and NICK are routed by who was in each channel beforehand.
        self.update_membership(s, msg, &me);
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
                    let tab = &mut self.servers[s].tabs[t];
                    if is_me(source) {
                        tab.joined = true;
                        tab.nicks.clear();
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
                let (old, new) = (irc_lower(source), irc_lower(new));
                for tab in &mut self.servers[s].tabs {
                    if tab.nicks.remove(&old) {
                        tab.nicks.insert(new.clone());
                    }
                }
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
                tab.nicks.clear();
            } else {
                tab.nicks.remove(&irc_lower(nick));
            }
        }
    }

    // ---- typed input ---------------------------------------------------------------------

    pub fn handle_input(&mut self, id: TabId, line: &str) {
        let Some((s, t)) = self.locate(id) else { return };
        let Some(input) = parse_input(line) else { return };
        let kind = self.servers[s].tabs[t].kind;
        let target = self.servers[s].tabs[t].name.clone();

        match input {
            Input::Say(text) | Input::Me(text) if kind == TabKind::Server => {
                let _ = text;
                self.line(s, t, "This is a server tab. Type /command to send a raw IRC command, e.g. /join #channel.");
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
                    self.send_raw(s, t, &sender, &format!("JOIN {args}"));
                }
            }
            Input::Part { channel, reason } => {
                let channel = match channel {
                    Some(channel) => channel,
                    None if kind == TabKind::Channel => target,
                    None => return self.line(s, t, "Not in a channel. Use /part #channel."),
                };
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
                    return self.line(s, t, "That's a channel. Use /join to join it.");
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
                    if let Err(e) = sender.send_quit(reason.unwrap_or_default()) {
                        self.line(s, t, &format!("Couldn't send: {e}"));
                    }
                }
            }
            Input::Close => match kind {
                TabKind::Server => self.line(s, t, "The server tab can't be closed. Use /quit to disconnect."),
                TabKind::Channel => {
                    if self.servers[s].tabs[t].joined {
                        if let Some(sender) = self.servers[s].sender.clone() {
                            let _ = net::send_raw(&sender, &format!("PART {target}"));
                        }
                    }
                    self.remove_tab(s, t);
                }
                TabKind::Query => self.remove_tab(s, t),
            },
            Input::Raw(line) => {
                if let Some(sender) = self.sender_or_complain(s, t) {
                    self.send_raw(s, t, &sender, &line);
                }
            }
            Input::Usage(text) => self.line(s, t, text),
        }
    }

    /// Sends a PRIVMSG (or ACTION) and shows it: in the target's tab if one is open,
    /// otherwise in the tab it was typed in.
    fn privmsg(&mut self, s: usize, t: usize, target: &str, text: &str, action: bool) {
        let Some(sender) = self.sender_or_complain(s, t) else { return };
        let result = if action { sender.send_action(target, text) } else { sender.send_privmsg(target, text) };
        if let Err(e) = result {
            return self.line(s, t, &format!("Couldn't send: {e}"));
        }

        let me = self.servers[s].nick.clone();
        let body = if action { format!("\x01ACTION {text}\x01") } else { text.to_string() };
        let echo = Message {
            tags: None,
            prefix: Some(Prefix::Nickname(me, String::new(), String::new())),
            command: Command::PRIVMSG(target.to_string(), body),
        };
        let Some(formatted) = format_message(&echo) else { return };
        match self.find_tab(s, target) {
            Some(dest) => self.line(s, dest, &formatted.text),
            None => self.line(s, t, &format!("-> {target}: {text}")),
        }
    }

    fn send_raw(&self, s: usize, t: usize, sender: &Sender, line: &str) {
        match net::send_raw(sender, line) {
            Ok(()) => {
                // Commands with their own visible result (JOIN, PART, NICK) don't need an echo.
                let quiet = ["JOIN ", "PART ", "NICK "].iter().any(|c| line.to_ascii_uppercase().starts_with(c));
                if !quiet {
                    self.line(s, t, &format!("-> {}", line.trim()));
                }
            }
            Err(e) => self.line(s, t, &format!("Couldn't send: {e}")),
        }
    }

    fn sender_or_complain(&self, s: usize, t: usize) -> Option<Sender> {
        let sender = self.servers[s].sender.clone();
        if sender.is_none() {
            self.line(s, t, "Not connected.");
        }
        sender
    }
}

/// Wires Enter in a tab's input box to `State::handle_input`.
fn bind_input(state: Weak<RefCell<State>>, id: TabId, tab: &Tab) {
    let input = tab.input;
    tab.input.on_text_enter(move |_| {
        let line = input.get_value();
        input.clear();
        if let Some(state) = state.upgrade() {
            state.borrow_mut().handle_input(id, &line);
        }
    });
}
