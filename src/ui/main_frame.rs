use wxdragon::prelude::*;

use crate::config::Config;

// Menu item ids. wx's stock ids are used where one exists.
const ID_MANAGE_SERVERS: i32 = 1001;
const ID_SETTINGS: i32 = 1002;
const ID_TOGGLE_SPEECH: i32 = 1101;
const ID_TOGGLE_ALL_SPEECH: i32 = 1102;

pub struct MainFrame {
    frame: Frame,
    notebook: Notebook,
}

impl MainFrame {
    pub fn new(config: &Config) -> Self {
        let frame = Frame::builder()
            .with_title("irctest")
            .with_size(Size::new(900, 650))
            .build();

        let notebook = Notebook::builder(&frame).build();

        let main = Self { frame, notebook };
        main.build_menus();
        main.add_placeholder_page(config);
        main.bind_menu_events();
        main
    }

    pub fn show(&self) {
        self.frame.show(true);
        self.frame.centre();
    }

    fn build_menus(&self) {
        let server_menu = Menu::builder()
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

    /// Until connections exist (step 4) the notebook gets one page so there's something to focus.
    fn add_placeholder_page(&self, config: &Config) {
        let panel = Panel::builder(&self.notebook).build();
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

        self.notebook.add_page(&panel, "Status", true, None);
        output.set_focus();
    }

    fn bind_menu_events(&self) {
        let frame = self.frame;
        self.frame.on_menu_selected(move |event: MenuEventData| match event.get_id() {
            ID_EXIT => {
                frame.close(true);
            }
            ID_ABOUT => {
                MessageDialog::builder(&frame, "irctest: an accessible IRC client.", "About irctest")
                    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
                    .build()
                    .show_modal();
            }
            ID_MANAGE_SERVERS | ID_SETTINGS | ID_TOGGLE_SPEECH | ID_TOGGLE_ALL_SPEECH => {
                MessageDialog::builder(&frame, "Not implemented yet.", "irctest")
                    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
                    .build()
                    .show_modal();
            }
            _ => event.skip(true),
        });
    }
}
