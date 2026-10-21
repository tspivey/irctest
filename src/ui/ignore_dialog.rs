//! Edit a working copy; nothing is applied or saved until OK.

use std::cell::RefCell;
use std::rc::Rc;

use irctest::config::ServerConfig;
use irctest::ignore;
use wxdragon::prelude::*;

use super::server_edit::labeled_text;

pub fn edit_ignores(
    parent: &dyn WxWidget,
    current: &[ServerConfig],
    selected: Option<usize>,
) -> Option<Vec<ServerConfig>> {
    let dialog = Dialog::builder(parent, "Ignore")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let help = StaticText::builder(&dialog)
        .with_label("Hide nicknames or channels on the selected server. Check names to remove their ignores.")
        .build();
    sizer.add(&help, 0, SizerFlag::All, 6);
    let server_label = StaticText::builder(&dialog).with_label("&Server").build();
    let server = ComboBox::builder(&dialog)
        .with_style(ComboBoxStyle::ReadOnly)
        .with_choices(current.iter().map(|s| s.name.clone()).collect())
        .build();
    if !current.is_empty() {
        server.set_selection(selected.unwrap_or(0).min(current.len() - 1) as u32);
    }
    server.enable(!current.is_empty());
    sizer.add(&server_label, 0, SizerFlag::All, 6);
    sizer.add(&server, 0, SizerFlag::Expand | SizerFlag::All, 6);
    let name = labeled_text(
        &dialog,
        &sizer,
        "&Nickname or channel",
        "",
        TextCtrlStyle::Default,
    );
    name.set_min_size(Size::new(420, -1));
    name.enable(!current.is_empty());
    let add = Button::builder(&dialog).with_label("&Add").build();
    add.enable(!current.is_empty());
    sizer.add(&add, 0, SizerFlag::All, 6);

    let list_label = StaticText::builder(&dialog)
        .with_label("Ignored names")
        .build();
    sizer.add(&list_label, 0, SizerFlag::All, 6);
    let list = ScrolledWindow::builder(&dialog)
        .with_style(ScrolledWindowStyle::VScroll)
        .build();
    list.set_min_size(Size::new(420, 200));
    list.set_scroll_rate(0, 16);
    let list_sizer = BoxSizer::builder(Orientation::Vertical).build();
    let empty = StaticText::builder(&list)
        .with_label(if current.is_empty() {
            "No servers configured."
        } else {
            "No ignored names on this server."
        })
        .build();
    list_sizer.add(&empty, 0, SizerFlag::All, 4);
    list.set_sizer(list_sizer, false);
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 6);
    let remove = Button::builder(&dialog)
        .with_label("&Remove ignore")
        .build();
    sizer.add(&remove, 0, SizerFlag::All, 6);

    let entries = Rc::new(RefCell::new(current.to_vec()));
    let checks: Rc<RefCell<Vec<CheckBox>>> = Rc::default();
    let refresh = {
        let entries = entries.clone();
        let checks = checks.clone();
        move || {
            for check in checks.borrow_mut().drain(..) {
                check.destroy();
            }
            let entries = entries.borrow();
            let names = server.get_selection().and_then(|i| entries.get(i as usize));
            empty.show(names.is_none_or(|s| s.ignored.is_empty()));
            remove.enable(false);
            if let Some(selected) = names {
                for entry in &selected.ignored {
                    // Ampersands are literal, never keyboard mnemonics.
                    let check = CheckBox::builder(&list)
                        .with_label(&entry.replace('&', "&&"))
                        .build();
                    list_sizer.add(&check, 0, SizerFlag::All, 4);
                    checks.borrow_mut().push(check);
                    let checks = Rc::downgrade(&checks);
                    check.on_toggled(move |_| {
                        if let Some(checks) = checks.upgrade() {
                            remove.enable(checks.borrow().iter().any(|c| c.get_value()));
                        }
                    });
                }
            }
            list.layout();
        }
    };
    refresh();
    {
        let refresh = refresh.clone();
        server.on_selection_changed(move |_| refresh());
    }
    {
        let (entries, refresh) = (entries.clone(), refresh.clone());
        add.on_click(move |_| {
            let Some(index) = server.get_selection() else {
                return;
            };
            if let Err(error) = ignore::add(
                &mut entries.borrow_mut()[index as usize].ignored,
                &name.get_value(),
            ) {
                MessageDialog::builder(&dialog, error, "Ignore")
                    .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning)
                    .build()
                    .show_modal();
            } else {
                refresh();
                name.set_value("");
            }
            name.set_focus();
        });
    }
    {
        let (entries, checks, refresh) = (entries.clone(), checks.clone(), refresh.clone());
        remove.on_click(move |_| {
            let Some(index) = server.get_selection() else {
                return;
            };
            let mut position = 0;
            entries.borrow_mut()[index as usize].ignored.retain(|_| {
                let keep = !checks.borrow()[position].get_value();
                position += 1;
                keep
            });
            refresh();
            if let Some(check) = checks.borrow().first() {
                check.set_focus();
            } else {
                name.set_focus();
            }
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
            if let Some(index) = server.get_selection() {
                if !name.get_value().trim().is_empty() {
                    if let Err(error) = ignore::add(
                        &mut entries.borrow_mut()[index as usize].ignored,
                        &name.get_value(),
                    ) {
                        MessageDialog::builder(&dialog, error, "Ignore")
                            .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconWarning)
                            .build()
                            .show_modal();
                        name.set_focus();
                        return;
                    }
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
    if current.is_empty() {
        cancel.set_focus();
    } else {
        server.set_focus();
    }
    let code = dialog.show_modal();
    dialog.destroy();
    if code == ID_OK {
        Some(entries.take())
    } else {
        None
    }
}
