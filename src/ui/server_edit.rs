//! Add / edit one server.

use std::cell::RefCell;
use std::rc::Rc;

use irctest::config::ServerConfig;
use irctest::route::irc_eq;
use wxdragon::prelude::*;

/// Shows the dialog; `Some` with the edited server if the user pressed OK.
/// `other_names` are the names of the other servers, which this one mustn't duplicate.
pub fn edit_server(parent: &dyn WxWidget, title: &str, server: &ServerConfig, other_names: &[String]) -> Option<ServerConfig> {
    let dialog = Dialog::builder(parent, title)
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    // A label just before each control gives it its accessible name.
    let name = labeled_text(&dialog, &sizer, "&Name", &server.name, TextCtrlStyle::Default);
    let host = labeled_text(&dialog, &sizer, "&Host", &server.host, TextCtrlStyle::Default);
    let port = labeled_text(&dialog, &sizer, "&Port", &server.port.to_string(), TextCtrlStyle::Default);
    let tls = CheckBox::builder(&dialog).with_label("Use &TLS").build();
    tls.set_value(server.tls);
    sizer.add(&tls, 0, SizerFlag::All, 6);
    let fingerprint_value = Rc::new(RefCell::new(server.fingerprint.clone()));
    // A group box rather than a label: NVDA reads a dialog's read-only single-line edits as its
    // description (Dialog.getDialogText), except one straight after a grouping. The box and the
    // controls are siblings, created in this order, so the field directly follows the box.
    let fingerprint_box = StaticBox::builder(&dialog).with_label("Trusted certificate &fingerprint").build();
    let fingerprint = TextCtrl::builder(&dialog).with_style(TextCtrlStyle::ReadOnly).build();
    // wx leaves read-only single-line text out of Tab navigation unless asked.
    fingerprint.enable_focus_from_keyboard();
    fingerprint.set_value(&fingerprint_text(&server.fingerprint));
    let delete_fingerprint = Button::builder(&dialog).with_label("&Delete fingerprint").build();
    delete_fingerprint.enable(!server.fingerprint.is_empty());
    let fingerprint_sizer = StaticBoxSizerBuilder::new_with_box(&fingerprint_box, Orientation::Vertical).build();
    fingerprint_sizer.add(&fingerprint, 0, SizerFlag::Expand | SizerFlag::All, 6);
    fingerprint_sizer.add(&delete_fingerprint, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom, 6);
    sizer.add_sizer(&fingerprint_sizer, 0, SizerFlag::Expand | SizerFlag::All, 6);
    // Neither wx nor Windows acts on a group box's mnemonic, so Alt+F is handled here.
    dialog.on_char_hook(move |event| {
        if let WindowEventData::Keyboard(key) = &event {
            if key.alt_down() && !key.control_down() && !key.shift_down() && key.get_key_code() == Some('F' as i32) {
                fingerprint.set_focus();
                event.skip(false);
                return;
            }
        }
        event.skip(true);
    });
    {
        let fingerprint_value = fingerprint_value.clone();
        delete_fingerprint.on_click(move |_| {
            fingerprint_value.borrow_mut().clear();
            fingerprint.set_value(&fingerprint_text(""));
            // The button is about to be disabled; don't leave focus on it.
            fingerprint.set_focus();
            delete_fingerprint.enable(false);
        });
    }
    let password = labeled_text(&dialog, &sizer, "Server pass&word", &server.password, TextCtrlStyle::Password);
    let nickname = labeled_text(
        &dialog,
        &sizer,
        "Nic&kname (leave blank to use the global nickname)",
        &server.nickname,
        TextCtrlStyle::Default,
    );
    let auto_connect = CheckBox::builder(&dialog).with_label("&Connect automatically at startup").build();
    auto_connect.set_value(server.auto_connect);
    sizer.add(&auto_connect, 0, SizerFlag::All, 6);
    let commands = labeled_text(
        &dialog,
        &sizer,
        "Commands to &send after connecting, one per line",
        &server.connect_commands.join("\n"),
        TextCtrlStyle::MultiLine,
    );
    commands.set_min_size(Size::new(400, 100));

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let ok = Button::builder(&dialog).with_id(ID_OK).with_label("OK").build();
    let cancel = Button::builder(&dialog).with_id(ID_CANCEL).with_label("Cancel").build();
    ok.set_default();
    buttons.add(&ok, 0, SizerFlag::All, 6);
    buttons.add(&cancel, 0, SizerFlag::All, 6);
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);

    let result: Rc<RefCell<Option<ServerConfig>>> = Rc::default();
    let other_names = other_names.to_vec();
    let ok_result = result.clone();
    ok.on_click(move |_| {
        let host_value = host.get_value().trim().to_string();
        if host_value.is_empty() {
            return complain(&dialog, "Enter the server's host name or address.", &host);
        }
        let Some(port_value) = parse_port(&port.get_value()) else {
            return complain(&dialog, "The port must be a number from 1 to 65535.", &port);
        };
        let name_value = match name.get_value().trim() {
            "" => host_value.clone(),
            n => n.to_string(),
        };
        if other_names.iter().any(|other| irc_eq(other, &name_value)) {
            return complain(&dialog, "Another server already has that name.", &name);
        }
        *ok_result.borrow_mut() = Some(ServerConfig {
            name: name_value,
            host: host_value,
            port: port_value,
            tls: tls.get_value(),
            password: password.get_value(),
            nickname: nickname.get_value().trim().to_string(),
            auto_connect: auto_connect.get_value(),
            connect_commands: parse_commands(&commands.get_value()),
            fingerprint: fingerprint_value.borrow().clone(),
        });
        dialog.end_modal(ID_OK);
    });

    dialog.set_sizer_and_fit(sizer, true);
    dialog.centre();
    name.set_focus();
    let code = dialog.show_modal();
    dialog.destroy();
    if code == ID_OK { result.take() } else { None }
}

/// Adds a label and a text control below it to `sizer`.
pub fn labeled_text(parent: &Dialog, sizer: &BoxSizer, label: &str, value: &str, style: TextCtrlStyle) -> TextCtrl {
    let label = StaticText::builder(parent).with_label(label).build();
    let text = TextCtrl::builder(parent).with_style(style).build();
    text.set_value(value);
    sizer.add(&label, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Top, 6);
    sizer.add(&text, 0, SizerFlag::Expand | SizerFlag::All, 6);
    text
}

fn complain(dialog: &Dialog, text: &str, field: &TextCtrl) {
    MessageDialog::builder(dialog, text, "irctest")
        .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning)
        .build()
        .show_modal();
    field.set_focus();
}

/// What the fingerprint box shows.
fn fingerprint_text(fingerprint: &str) -> String {
    if fingerprint.is_empty() { "None".into() } else { fingerprint.into() }
}

fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse::<u16>().ok().filter(|p| *p != 0)
}

/// One command per non-blank line, trimmed.
fn parse_commands(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(parse_port("6667"), Some(6667));
        assert_eq!(parse_port(" 6697 "), Some(6697));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("70000"), None);
        assert_eq!(parse_port("abc"), None);
        assert_eq!(parse_port(""), None);
    }

    #[test]
    fn commands() {
        assert_eq!(parse_commands("JOIN #a\r\n\r\n  PRIVMSG x :y  \n"), vec!["JOIN #a", "PRIVMSG x :y"]);
        assert!(parse_commands("  \n").is_empty());
    }
}
