use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, mpsc};

use irctest::config::Config;
use irctest::net::{ConnectionEvent, EventSink, NetEvent, ServerId};
use tokio::runtime::Handle;
use wxdragon::prelude::*;

use super::cert_dialog::ask_certificate;
use super::ignore_dialog::edit_ignores;
use super::richedit::disable_beep;
use super::server_dialog::manage_servers;
use super::settings_dialog::{Settings, edit_settings};
use super::state::State;

// Menu item ids. wx's stock ids are used where one exists.
const ID_DISCONNECT: i32 = 1000;
const ID_MANAGE_SERVERS: i32 = 1001;
const ID_SETTINGS: i32 = 1002;
const ID_RECONNECT: i32 = 1003;
const ID_IGNORE: i32 = 1004;
const ID_CLOSE_TAB: i32 = 1103;
const ID_TOGGLE_SPEECH: i32 = 1101;
const ID_TOGGLE_ALL_SPEECH: i32 = 1102;
const ID_NEXT_TAB: i32 = 1104;
const ID_PREVIOUS_TAB: i32 = 1105;
const ID_NEXT_NETWORK: i32 = 1106;
const ID_PREVIOUS_NETWORK: i32 = 1107;
const ID_STOP_SPEECH: i32 = 1108;
const ID_SWITCH_PANE: i32 = 1109;

thread_local! {
    /// Drains network events on the GUI thread. Set once by `MainFrame::new`.
    static NET_HANDLER: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
}

/// Runs on the network threads: queue the event, then ask the GUI thread to drain the queue.
fn make_sink(tx: mpsc::Sender<(ServerId, ConnectionEvent)>) -> EventSink {
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

pub struct MainFrame {
    frame: Frame,
    state: Rc<RefCell<State>>,
}

impl MainFrame {
    pub fn new(
        config: Config,
        runtime: Handle,
        sync_data: irctest::sync_state::SyncData,
        writer: irctest::sync_state::Writer,
        sync_error: Option<String>,
    ) -> Self {
        let frame = Frame::builder()
            .with_title("irctest")
            .with_size(Size::new(900, 650))
            .build();
        let notebook = Notebook::builder(&frame).build();

        let (tx, rx) = mpsc::channel();
        let placeholder = add_placeholder_page(&notebook, &config);
        let state = State::new(
            config,
            runtime,
            notebook,
            make_sink(tx),
            placeholder,
            sync_data,
            writer,
            sync_error,
        );

        let drain_state = state.clone();
        NET_HANDLER.with(|handler| {
            *handler.borrow_mut() = Some(Box::new(move || {
                while let Ok((id, event)) = rx.try_recv() {
                    let attempt = event.attempt;
                    if !drain_state.borrow().accepts_event(id, attempt) {
                        if let NetEvent::UntrustedCertificate(p) = event.event {
                            p.answer(false);
                        }
                        continue;
                    }
                    match event.event {
                        // State isn't borrowed while the dialog runs. Other events wait in the
                        // queue until it closes: this drain is still running.
                        NetEvent::UntrustedCertificate(prompt) => {
                            let Some((name, saved)) =
                                drain_state.borrow_mut().certificate_prompt(id, &prompt)
                            else {
                                prompt.answer(false);
                                continue;
                            };
                            let choice = ask_certificate(&frame, &name, &saved, &prompt);
                            if drain_state.borrow().accepts_event(id, attempt) {
                                drain_state
                                    .borrow_mut()
                                    .certificate_answered(id, prompt, choice);
                            } else {
                                prompt.answer(false);
                            }
                        }
                        event => drain_state.borrow_mut().handle_net_event(id, event),
                    }
                }
            }));
        });

        let close_state = state.clone();
        frame.on_close(move |event| {
            close_state.borrow_mut().shutdown();
            event.skip(true);
        });
        let destroy_state = state.clone();
        frame.on_destroy(move |event| {
            destroy_state.borrow_mut().shutdown();
            event.skip(true);
        });
        let main = Self { frame, state };
        main.build_menus();
        main.bind_menu_events();
        main.bind_navigation();
        main
    }

    pub fn show(&self) {
        self.frame.show(true);
        self.frame.centre();
    }

    /// Connects the servers marked auto-connect. Call after `show`.
    pub fn connect_auto(&self) {
        self.state.borrow_mut().connect_auto();
    }

    fn build_menus(&self) {
        let server_menu = Menu::builder()
            .append_item(
                ID_MANAGE_SERVERS,
                "&Manage servers...\tCtrl+Shift+S",
                "Add, edit, delete and connect to servers",
            )
            .append_item(
                ID_DISCONNECT,
                "&Disconnect",
                "Disconnect from the current tab's server",
            )
            .append_item(
                ID_RECONNECT,
                "&Reconnect",
                "Reconnect to the current tab's server now",
            )
            .append_item(
                ID_SETTINGS,
                "Se&ttings...",
                "Global nickname and other settings",
            )
            .append_item(
                ID_IGNORE,
                "&Ignore...",
                "Hide text from nicknames or channels",
            )
            .append_separator()
            .append_item(ID_EXIT, "E&xit\tAlt+F4", "Quit irctest")
            .build();

        let view_menu = Menu::builder()
            .append_item(ID_NEXT_TAB, "&Next tab\tCtrl+Tab", "Select the next tab")
            .append_item(
                ID_PREVIOUS_TAB,
                "&Previous tab\tCtrl+Shift+Tab",
                "Select the previous tab",
            )
            .append_item(
                ID_NEXT_NETWORK,
                "Next netw&ork\tCtrl+PageDown",
                "Select the next server, account or network",
            )
            .append_item(
                ID_PREVIOUS_NETWORK,
                "Previous net&work\tCtrl+PageUp",
                "Select the previous server, account or network",
            )
            .append_item(
                ID_SWITCH_PANE,
                "Switch &focus between Message and Output\tF6",
                "Move focus between this tab's Message input and Output",
            )
            .append_separator()
            .append_item(
                ID_STOP_SPEECH,
                "S&top speaking\tF4",
                "Silence whatever is being spoken now",
            )
            .append_item(
                ID_TOGGLE_SPEECH,
                "Toggle &speech for this tab\tF5",
                "Turn speech on or off for the current tab",
            )
            .append_item(
                ID_TOGGLE_ALL_SPEECH,
                "Toggle &all speech\tShift+F5",
                "Turn all speech on or off",
            )
            .append_separator()
            .append_item(
                ID_CLOSE_TAB,
                "&Close tab\tCtrl+F4",
                "Leave the channel or close the private messages",
            )
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
        self.frame
            .on_menu_selected(move |event: MenuEventData| match event.get_id() {
                // State is never borrowed while a dialog runs: network events are handled during
                // the dialog's event loop and need to borrow it.
                ID_MANAGE_SERVERS => {
                    let config = state.borrow().config().clone();
                    let saved = config.clone();
                    let result = manage_servers(&frame, &config, move |servers| {
                        let config = Config {
                            servers: servers.to_vec(),
                            ..saved.clone()
                        };
                        config.save().map_err(|e| e.to_string())
                    });
                    let config = Config {
                        servers: result.servers,
                        ..config
                    };
                    let connect = result.connect.map(|i| config.servers[i].clone());
                    state.borrow_mut().set_config(config);
                    if let Some(server) = connect {
                        state.borrow_mut().connect(server);
                    }
                }
                ID_SETTINGS => {
                    let config = state.borrow().config().clone();
                    let current = Settings {
                        nickname: config.nickname.clone(),
                        username: config.username.clone(),
                        realname: config.realname.clone(),
                        log: config.log,
                    };
                    if let Some(new) = edit_settings(&frame, &current) {
                        let config = Config {
                            nickname: new.nickname,
                            username: new.username,
                            realname: new.realname,
                            log: new.log,
                            ..config
                        };
                        if let Err(e) = config.save() {
                            message(&frame, &format!("Couldn't save settings:\n\n{e}"));
                        }
                        state.borrow_mut().set_config(config);
                    }
                }
                ID_IGNORE => {
                    let (ignored, channels) = {
                        let state = state.borrow();
                        (state.config().ignored.clone(), state.ignore_channels())
                    };
                    if let Some(ignored) = edit_ignores(&frame, &ignored, &channels) {
                        // Events can update settings while the modal dialog is open.
                        let mut config = state.borrow().config().clone();
                        config.ignored = ignored;
                        if let Err(e) = config.save() {
                            message(&frame, &format!("Couldn't save ignores:\n\n{e}"));
                        }
                        state.borrow_mut().set_config(config);
                    }
                }
                ID_RECONNECT => {
                    let result = state.borrow_mut().reconnect_current();
                    if let Err(e) = result {
                        message(&frame, &e);
                    }
                }
                ID_CLOSE_TAB => state.borrow_mut().close_current(),
                ID_DISCONNECT => {
                    let result = state.borrow_mut().disconnect_current();
                    if let Err(e) = result {
                        message(&frame, &e);
                    }
                }
                ID_EXIT => {
                    frame.close(true);
                }
                ID_ABOUT => message(&frame, "irctest: an accessible IRC client."),
                ID_STOP_SPEECH => state.borrow_mut().stop_speech(),
                ID_SWITCH_PANE => state.borrow().switch_pane(),
                ID_TOGGLE_SPEECH => state.borrow_mut().toggle_tab_speech(),
                ID_TOGGLE_ALL_SPEECH => state.borrow_mut().toggle_all_speech(),
                ID_NEXT_TAB => state.borrow().navigate_tab(true),
                ID_PREVIOUS_TAB => state.borrow().navigate_tab(false),
                ID_NEXT_NETWORK => state.borrow().navigate_network(true),
                ID_PREVIOUS_NETWORK => state.borrow().navigate_network(false),
                _ => event.skip(true),
            });
    }

    fn bind_navigation(&self) {
        let state = self.state.clone();
        // Catch these before the focused edit control or native notebook handles
        // them. GTK's default Ctrl+Tab can focus the strip without changing pages.
        self.frame.on_char_hook(move |event| {
            if let WindowEventData::Keyboard(key) = &event {
                if key.control_down() && !key.alt_down() {
                    match key.get_key_code() {
                        Some(WXK_TAB) => {
                            event.skip(false);
                            state.borrow().navigate_tab(!key.shift_down());
                            return;
                        }
                        Some(WXK_PAGEDOWN | WXK_PAGEUP) if !key.shift_down() => {
                            event.skip(false);
                            state
                                .borrow()
                                .navigate_network(key.get_key_code() == Some(WXK_PAGEDOWN));
                            return;
                        }
                        _ => {}
                    }
                }
            }
            event.skip(true);
        });
    }
}

/// Shown until the first server tab exists, so the notebook has something to focus.
fn add_placeholder_page(notebook: &Notebook, config: &Config) -> Panel {
    let panel = Panel::builder(notebook).build();
    let output = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::Rich2)
        .build();
    disable_beep(&output);
    let servers = match config.servers.len() {
        1 => "1 server".to_string(),
        n => format!("{n} servers"),
    };
    let nick = if config.nickname.is_empty() {
        "not set"
    } else {
        &config.nickname
    };
    output.set_value(&format!(
        "Not connected. {servers} configured. Global nickname: {nick}."
    ));
    output.set_insertion_point(output.get_last_position());

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
