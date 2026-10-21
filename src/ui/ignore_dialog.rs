//! Edit a working copy; nothing is applied or saved until OK.

use std::cell::RefCell;
use std::rc::Rc;

use irctest::ignore;
use irctest::route::irc_eq;
use wxdragon::prelude::*;

use super::server_edit::labeled_text;

pub fn edit_ignores(
    parent: &dyn WxWidget,
    current: &[String],
    channels: &[String],
) -> Option<Vec<String>> {
    let dialog = Dialog::builder(parent, "Ignore")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let help = StaticText::builder(&dialog)
        .with_label("Hide messages from these nicknames or channels on all networks.")
        .build();
    sizer.add(&help, 0, SizerFlag::All, 6);
    let name = labeled_text(
        &dialog,
        &sizer,
        "&Nickname or channel",
        "",
        TextCtrlStyle::Default,
    );
    name.set_min_size(Size::new(420, -1));
    let add = Button::builder(&dialog).with_label("&Add").build();
    sizer.add(&add, 0, SizerFlag::All, 6);

    let channel_label = StaticText::builder(&dialog).with_label("Channels").build();
    sizer.add(&channel_label, 0, SizerFlag::All, 6);
    let channel_window = ScrolledWindow::builder(&dialog)
        .with_style(ScrolledWindowStyle::VScroll)
        .build();
    channel_window.set_min_size(Size::new(420, 140));
    channel_window.set_scroll_rate(0, 16);
    let channel_sizer = BoxSizer::builder(Orientation::Vertical).build();
    let mut checks = Vec::new();
    for channel in channels {
        // Ampersands in channel names are literal, never keyboard mnemonics.
        let label = format!("Ignore this channel: {}", channel.replace('&', "&&"));
        let check = CheckBox::builder(&channel_window)
            .with_label(&label)
            .build();
        check.set_value(ignore::contains(current, channel));
        channel_sizer.add(&check, 0, SizerFlag::All, 4);
        checks.push((channel.clone(), check));
    }
    if checks.is_empty() {
        let empty = StaticText::builder(&channel_window)
            .with_label("No channel tabs are open.")
            .build();
        channel_sizer.add(&empty, 0, SizerFlag::All, 4);
    }
    channel_window.set_sizer(channel_sizer, true);
    sizer.add(&channel_window, 1, SizerFlag::Expand | SizerFlag::All, 6);

    let list_label = StaticText::builder(&dialog)
        .with_label("&Ignored names")
        .build();
    let list = ListBox::builder(&dialog).build();
    list.set_min_size(Size::new(420, 120));
    let remove = Button::builder(&dialog).with_label("&Remove").build();
    sizer.add(&list_label, 0, SizerFlag::All, 6);
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 6);
    sizer.add(&remove, 0, SizerFlag::All, 6);

    let entries = Rc::new(RefCell::new(current.to_vec()));
    let refresh = {
        let entries = entries.clone();
        let checks = checks.clone();
        move |selection: Option<usize>| {
            let entries = entries.borrow();
            let items: Vec<&str> = entries.iter().map(String::as_str).collect();
            list.set_items(&items);
            if !entries.is_empty() {
                list.set_selection(selection.unwrap_or(0).min(entries.len() - 1) as u32, true);
            }
            remove.enable(!entries.is_empty());
            for (channel, check) in &checks {
                check.set_value(ignore::contains(&entries, channel));
            }
        }
    };
    refresh(None);
    {
        let (entries, refresh) = (entries.clone(), refresh.clone());
        add.on_click(move |_| {
            if let Err(error) = ignore::add(&mut entries.borrow_mut(), &name.get_value()) {
                MessageDialog::builder(&dialog, error, "Ignore")
                    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning)
                    .build()
                    .show_modal();
            } else {
                refresh(Some(entries.borrow().len().saturating_sub(1)));
                name.set_value("");
            }
            name.set_focus();
        });
    }
    for (channel, check) in checks {
        let (entries, refresh) = (entries.clone(), refresh.clone());
        check.on_toggled(move |event| {
            if event.is_checked() {
                let _ = ignore::add(&mut entries.borrow_mut(), &channel);
            } else {
                entries
                    .borrow_mut()
                    .retain(|entry| !irc_eq(entry.trim(), &channel));
            }
            refresh(None);
        });
    }
    {
        let (entries, refresh) = (entries.clone(), refresh.clone());
        remove.on_click(move |_| {
            if let Some(index) = list.get_selection() {
                entries.borrow_mut().remove(index as usize);
                refresh(Some(index as usize));
            }
            list.set_focus();
        });
    }

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let ok = Button::builder(&dialog)
        .with_id(ID_OK)
        .with_label("OK")
        .build();
    let cancel = Button::builder(&dialog)
        .with_id(ID_CANCEL)
        .with_label("Cancel")
        .build();
    ok.set_default();
    // Also accept a name still in the field, so typing a name and pressing OK works.
    {
        let entries = entries.clone();
        ok.on_click(move |_| {
            if !name.get_value().trim().is_empty() {
                if let Err(error) = ignore::add(&mut entries.borrow_mut(), &name.get_value()) {
                    MessageDialog::builder(&dialog, error, "Ignore")
                        .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning)
                        .build()
                        .show_modal();
                    name.set_focus();
                    return;
                }
            }
            dialog.end_modal(ID_OK);
        });
    }
    buttons.add(&ok, 0, SizerFlag::All, 6);
    buttons.add(&cancel, 0, SizerFlag::All, 6);
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);
    dialog.set_sizer_and_fit(sizer, true);
    dialog.centre();
    name.set_focus();
    let code = dialog.show_modal();
    dialog.destroy();
    if code == ID_OK {
        Some(entries.take())
    } else {
        None
    }
}
