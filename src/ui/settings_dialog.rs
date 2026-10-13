//! Global settings: nickname, username, real name, logging.

use std::cell::RefCell;
use std::rc::Rc;

use wxdragon::prelude::*;

use super::server_edit::labeled_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub nickname: String,
    pub username: String,
    pub realname: String,
    pub log: bool,
}

/// Shows the dialog; `Some` with the new settings if the user pressed OK.
pub fn edit_settings(parent: &dyn WxWidget, current: &Settings) -> Option<Settings> {
    let dialog = Dialog::builder(parent, "Settings").build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let nickname = labeled_text(&dialog, &sizer, "&Nickname", &current.nickname, TextCtrlStyle::Default);
    let username = labeled_text(
        &dialog,
        &sizer,
        "&Username (leave blank to use the nickname)",
        &current.username,
        TextCtrlStyle::Default,
    );
    let realname = labeled_text(
        &dialog,
        &sizer,
        "&Real name (leave blank to use the nickname)",
        &current.realname,
        TextCtrlStyle::Default,
    );
    let log = CheckBox::builder(&dialog).with_label("&Log conversations").build();
    log.set_value(current.log);
    sizer.add(&log, 0, SizerFlag::All, 6);
    nickname.set_min_size(Size::new(320, -1));

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let ok = Button::builder(&dialog).with_id(ID_OK).with_label("OK").build();
    let cancel = Button::builder(&dialog).with_id(ID_CANCEL).with_label("Cancel").build();
    ok.set_default();
    buttons.add(&ok, 0, SizerFlag::All, 6);
    buttons.add(&cancel, 0, SizerFlag::All, 6);
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);

    let result: Rc<RefCell<Option<Settings>>> = Rc::default();
    let ok_result = result.clone();
    ok.on_click(move |_| {
        *ok_result.borrow_mut() = Some(Settings {
            nickname: nickname.get_value().trim().to_string(),
            username: username.get_value().trim().to_string(),
            realname: realname.get_value().trim().to_string(),
            log: log.get_value(),
        });
        dialog.end_modal(ID_OK);
    });

    dialog.set_sizer_and_fit(sizer, true);
    dialog.centre();
    nickname.set_focus();
    let code = dialog.show_modal();
    dialog.destroy();
    if code == ID_OK { result.take() } else { None }
}
