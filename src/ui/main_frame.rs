use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, mpsc};

use irc::client::Sender;
use irctest::config::{Config, ServerConfig};
use irctest::format::format_message;
use irctest::net::{self, EventSink, NetEvent, ServerId};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;
use wxdragon::prelude::*;

use super::tab::Tab;

// Menu item ids. wx's stock ids are used where one exists.
const ID_CONNECT: i32 = 1000;
const ID_MANAGE_SERVERS: i32 = 1001;
const ID_SETTINGS: i32 = 1002;
const ID_TOGGLE_SPEECH: i32 = 1101;
const ID_TOGGLE_ALL_SPEECH: i32 = 1102;

thread_local! {
    /// Drains network events on the GUI thread. Set once by `MainFrame::new`.
    static NET_HANDLER: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
}

/// Runs on the network threads: queue the event, then ask the GUI thread to drain the queue.
fn make_sink(tx: mpsc::Sender<(ServerId, NetEvent)>) -> EventSink {
    Arc::new(move |id, event| {
        if tx.send((id, event)).is_ok() {
            wxdragon::call_after(Box::new(|| {
                NET_HANDLER.with(|handler| {
                    // Already draining further up the stack (a nested event loop): that drain
                    // will pick this event up too.
                    if let Ok(mut handler) = handler.try_borrow_mut() {
                        if let Some(drain) = handler.as_mut() {
                            drain();
                        }
                    }
                });
            }));
            // call_after only queues; this makes an idle GUI process the queue now.
            wxdragon::wake_up_idle();
        }
    })
}

struct Server {
    id: ServerId,
    config: ServerConfig,
    tab: Tab,
    /// Set once the connection is up; cleared when it drops.
    sender: Option<Sender>,
    /// Set while connecting or connected.
    task: Option<AbortHandle>,
    nick: String,
}

struct State {
    config: Config,
    runtime: Handle,
    notebook: Notebook,
    sink: EventSink,
    servers: Vec<Server>,
    next_id: ServerId,
    /// The "Not connected" page shown before any server tab exists.
    placeholder: Option<Panel>,
}

pub struct MainFrame {
    frame: Frame,
    state: Rc<RefCell<State>>,
}

impl MainFrame {
    pub fn new(config: Config, runtime: Handle) -> Self {
        let frame = Frame::builder()
            .with_title("irctest")
            .with_size(Size::new(900, 650))
            .build();
        let notebook = Notebook::builder(&frame).build();

        let (tx, rx) = mpsc::channel();
        let placeholder = add_placeholder_page(&notebook, &config);
        let state = Rc::new(RefCell::new(State {
            config,
            runtime,
            notebook,
            sink: make_sink(tx),
            servers: Vec::new(),
            next_id: 1,
            placeholder: Some(placeholder),
        }));

        let drain_state = state.clone();
        NET_HANDLER.with(|handler| {
            *handler.borrow_mut() = Some(Box::new(move || {
                while let Ok((id, event)) = rx.try_recv() {
                    drain_state.borrow_mut().handle_net_event(id, event);
                }
            }));
        });

        let main = Self { frame, state };
        main.build_menus();
        main.bind_menu_events();
        main
    }

    pub fn show(&self) {
        self.frame.show(true);
        self.frame.centre();
    }

    fn build_menus(&self) {
        let server_menu = Menu::builder()
            .append_item(ID_CONNECT, "&Connect", "Connect to the first server in irctest.toml")
            .append_item(ID_MANAGE_SERVERS, "&Manage servers...\tCtrl+Shift+S", "Add, edit, delete and connect to servers")
            .append_item(ID_SETTINGS, "Se&ttings...", "Global nickname and other settings")
            .append_separator()
            .append_item(ID_EXIT, "E&xit\tAlt+F4", "Quit irctest")
            .build();

        let view_menu = Menu::builder()
            .append_item(ID_TOGGLE_SPEECH, "Toggle &speech for this tab\tF5", "Turn speech on or off for the current tab")
            .append_item(ID_TOGGLE_ALL_SPEECH, "Toggle &all speech\tShift+F5", "Turn all speech on or off")
            .build();

        let help_menu = Menu::builder()
            .append_item(ID_ABOUT, "&About irctest", "About irctest")
            .build();

        let menu_bar = MenuBar::builder()
            .append(server_menu, "&Server")
            .append(view_menu, "&View")
            .append(help_menu, "&Help")
            .build();

        self.frame.set_menu_bar(menu_bar);
    }

    fn bind_menu_events(&self) {
        let frame = self.frame;
        let state = self.state.clone();
        self.frame.on_menu_selected(move |event: MenuEventData| match event.get_id() {
            ID_CONNECT => {
                // Bind the result first so the state borrow ends before any dialog runs.
                let result = State::connect_first(&state);
                if let Err(e) = result {
                    message(&frame, &e);
                }
            }
            ID_EXIT => {
                frame.close(true);
            }
            ID_ABOUT => message(&frame, "irctest: an accessible IRC client."),
            ID_MANAGE_SERVERS | ID_SETTINGS | ID_TOGGLE_SPEECH | ID_TOGGLE_ALL_SPEECH => {
                message(&frame, "Not implemented yet.")
            }
            _ => event.skip(true),
        });
    }
}

impl State {
    /// Temporary until the server manager (step 6): connect to the first configured server.
    fn connect_first(state: &Rc<RefCell<State>>) -> Result<(), String> {
        let mut st = state.borrow_mut();
        let server_config = st.config.servers.first().cloned().ok_or("There are no servers in irctest.toml.")?;

        // Reuse the server's tab if it has one.
        let index = match st.servers.iter().position(|s| s.config.name == server_config.name) {
            Some(i) => i,
            None => {
                let id = st.next_id;
                st.next_id += 1;
                let tab = st.add_server_tab(&server_config);
                bind_input(state, id, &tab);
                st.servers.push(Server { id, config: server_config, tab, sender: None, task: None, nick: String::new() });
                st.servers.len() - 1
            }
        };

        let st = &mut *st;
        let server = &mut st.servers[index];
        if server.task.is_some() {
            server.tab.append_line("Already connected.");
        } else {
            let nick = st.config.effective_nick(&server.config);
            server.tab.append_line(&format!(
                "Connecting to {}:{} as {}...",
                server.config.host, server.config.port, nick
            ));
            server.task = Some(net::connect(&st.runtime, server.id, &st.config, &server.config, st.sink.clone()));
        }
        Ok(())
    }

    fn add_server_tab(&mut self, server: &ServerConfig) -> Tab {
        if let Some(placeholder) = self.placeholder.take() {
            self.notebook.remove_page(0);
            placeholder.destroy();
        }
        let tab = Tab::new(&self.notebook);
        let label = if server.name.is_empty() { &server.host } else { &server.name };
        self.notebook.add_page(&tab.panel, label, true, None);
        tab.input.set_focus();
        tab
    }

    fn server(&self, id: ServerId) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }

    fn handle_net_event(&mut self, id: ServerId, event: NetEvent) {
        let Some(server) = self.servers.iter_mut().find(|s| s.id == id) else { return };
        match event {
            NetEvent::Connected(sender) => {
                server.sender = Some(sender);
                server.tab.append_line("Connected.");
            }
            NetEvent::Message(msg) => {
                if let Some(formatted) = format_message(&msg) {
                    server.tab.append_line(&formatted.text);
                }
            }
            NetEvent::NickChanged(nick) => server.nick = nick,
            NetEvent::Error(e) => server.tab.append_line(&format!("Error: {e}")),
            NetEvent::Disconnected(reason) => {
                server.sender = None;
                server.task = None;
                server.tab.append_line(&format!("Disconnected: {reason}"));
            }
        }
    }

    /// Text typed into a server tab's input box.
    fn handle_input(&self, id: ServerId, line: &str) {
        let Some(server) = self.server(id) else { return };
        if line.trim().is_empty() {
            return;
        }
        let Some(command) = line.strip_prefix('/') else {
            server.tab.append_line("This is a server tab. Type /command to send a raw IRC command, e.g. /join #channel.");
            return;
        };
        let raw = strip_word(command, "raw").or_else(|| strip_word(command, "quote")).unwrap_or(command);
        match &server.sender {
            None => server.tab.append_line("Not connected."),
            Some(sender) => match net::send_raw(sender, raw) {
                Ok(()) => server.tab.append_line(&format!("-> {}", raw.trim())),
                Err(e) => server.tab.append_line(&format!("Couldn't send: {e}")),
            },
        }
    }
}

/// Wires Enter in a tab's input box to `State::handle_input`.
fn bind_input(state: &Rc<RefCell<State>>, id: ServerId, tab: &Tab) {
    let state = Rc::downgrade(state);
    let input = tab.input;
    tab.input.on_text_enter(move |_| {
        let line = input.get_value();
        input.clear();
        if let Some(state) = state.upgrade() {
            state.borrow().handle_input(id, &line);
        }
    });
}

/// `"raw PRIVMSG x :y"` with word `"raw"` gives `Some("PRIVMSG x :y")`. Case-insensitive.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let (first, rest) = text.split_once(' ').unwrap_or((text, ""));
    first.eq_ignore_ascii_case(word).then_some(rest)
}

/// Shown until the first server tab exists, so the notebook has something to focus.
fn add_placeholder_page(notebook: &Notebook, config: &Config) -> Panel {
    let panel = Panel::builder(notebook).build();
    let output = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::Rich2)
        .build();
    let servers = match config.servers.len() {
        1 => "1 server".to_string(),
        n => format!("{n} servers"),
    };
    let nick = if config.nickname.is_empty() { "not set" } else { &config.nickname };
    output.set_value(&format!("Not connected. {servers} configured. Global nickname: {nick}."));

    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&output, 1, SizerFlag::Expand | SizerFlag::All, 4);
    panel.set_sizer(sizer, true);

    notebook.add_page(&panel, "Status", true, None);
    output.set_focus();
    panel
}

fn message(frame: &Frame, text: &str) {
    MessageDialog::builder(frame, text, "irctest")
        .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
        .build()
        .show_modal();
}

#[cfg(test)]
mod tests {
    use super::strip_word;

    #[test]
    fn strip_word_cases() {
        assert_eq!(strip_word("raw PRIVMSG x :y", "raw"), Some("PRIVMSG x :y"));
        assert_eq!(strip_word("RAW JOIN #a", "raw"), Some("JOIN #a"));
        assert_eq!(strip_word("rawness", "raw"), None);
        assert_eq!(strip_word("join #a", "raw"), None);
        assert_eq!(strip_word("raw", "raw"), Some(""));
    }
}
