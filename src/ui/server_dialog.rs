//! Server manager: the list of servers with Connect, Add, Edit, Delete and Close.

use std::cell::RefCell;
use std::rc::Rc;

use irctest::config::{Config, ServerConfig};
use wxdragon::prelude::*;

use super::server_edit::edit_server;

const ID_CONNECT: i32 = 2001;

pub struct ManageResult {
    /// The server list after any adds, edits and deletes (already saved by the caller's `save`).
    pub servers: Vec<ServerConfig>,
    /// Index into `servers` of the server to connect to, if Connect was pressed.
    pub connect: Option<usize>,
}

/// Shows the server manager. `save` is called after every change so nothing is lost if the
/// app is closed with the dialog open; it returns an error message to show on failure.
pub fn manage_servers(parent: &dyn WxWidget, config: &Config, save: impl Fn(&[ServerConfig]) -> Result<(), String> + 'static) -> ManageResult {
    let dialog = Dialog::builder(parent, "Manage servers")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .build();
    let servers = Rc::new(RefCell::new(config.servers.clone()));

    let label = StaticText::builder(&dialog).with_label("&Servers").build();
    let list = ListBox::builder(&dialog).build();
    list.set_min_size(Size::new(360, 200));

    // Connect comes first in tab order, right after the list.
    let connect = Button::builder(&dialog).with_id(ID_CONNECT).with_label("&Connect").build();
    let add = Button::builder(&dialog).with_label("&Add...").build();
    let edit = Button::builder(&dialog).with_label("&Edit...").build();
    let delete = Button::builder(&dialog).with_label("&Delete").build();
    let close = Button::builder(&dialog).with_id(ID_CANCEL).with_label("Close").build();
    // Enter in the list presses the default button, so it connects.
    connect.set_default();

    let refresh = {
        let servers = servers.clone();
        move |select: Option<usize>| {
            let servers = servers.borrow();
            let items: Vec<String> = servers.iter().map(describe).collect();
            let items: Vec<&str> = items.iter().map(String::as_str).collect();
            list.set_items(&items);
            let selected = select.filter(|&i| i < servers.len()).or(if servers.is_empty() { None } else { Some(0) });
            if let Some(i) = selected {
                list.set_selection(i as u32, true);
            }
            for button in [connect, edit, delete] {
                button.enable(selected.is_some());
            }
        }
    };
    refresh(None);

    let enable_for_selection = move || {
        let selected = list.get_selection().is_some();
        for button in [connect, edit, delete] {
            button.enable(selected);
        }
    };
    list.on_selection_changed(move |_| enable_for_selection());

    let save = Rc::new(save);
    let connect_index: Rc<RefCell<Option<usize>>> = Rc::default();

    {
        let connect_index = connect_index.clone();
        connect.on_click(move |_| {
            if let Some(i) = list.get_selection() {
                *connect_index.borrow_mut() = Some(i as usize);
                dialog.end_modal(ID_CONNECT);
            }
        });
    }
    {
        let connect_index = connect_index.clone();
        list.on_item_double_clicked(move |_| {
            if let Some(i) = list.get_selection() {
                *connect_index.borrow_mut() = Some(i as usize);
                dialog.end_modal(ID_CONNECT);
            }
        });
    }
    {
        let (servers, save, refresh) = (servers.clone(), save.clone(), refresh.clone());
        add.on_click(move |_| {
            let names: Vec<String> = servers.borrow().iter().map(|s| s.name.clone()).collect();
            let new = ServerConfig::default();
            if let Some(server) = edit_server(&dialog, "Add server", &new, &names) {
                servers.borrow_mut().push(server);
                let index = servers.borrow().len() - 1;
                save_or_complain(&dialog, &*save, &servers.borrow());
                refresh(Some(index));
            }
            list.set_focus();
        });
    }
    {
        let (servers, save, refresh) = (servers.clone(), save.clone(), refresh.clone());
        edit.on_click(move |_| {
            let Some(index) = list.get_selection().map(|i| i as usize) else { return };
            let current = servers.borrow()[index].clone();
            let names: Vec<String> =
                servers.borrow().iter().enumerate().filter(|(i, _)| *i != index).map(|(_, s)| s.name.clone()).collect();
            if let Some(server) = edit_server(&dialog, "Edit server", &current, &names) {
                servers.borrow_mut()[index] = server;
                save_or_complain(&dialog, &*save, &servers.borrow());
                refresh(Some(index));
            }
            list.set_focus();
        });
    }
    {
        let (servers, save, refresh) = (servers.clone(), save.clone(), refresh.clone());
        delete.on_click(move |_| {
            let Some(index) = list.get_selection().map(|i| i as usize) else { return };
            let name = servers.borrow()[index].name.clone();
            let answer = MessageDialog::builder(&dialog, &format!("Delete the server {name}?"), "Delete server")
                .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::NoDefault | MessageDialogStyle::IconQuestion)
                .build()
                .show_modal();
            if answer == ID_YES {
                servers.borrow_mut().remove(index);
                save_or_complain(&dialog, &*save, &servers.borrow());
                refresh(Some(index.saturating_sub(1)));
            }
            list.set_focus();
        });
    }

    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&label, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Top, 6);
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 6);
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    for button in [connect, add, edit, delete, close] {
        buttons.add(&button, 0, SizerFlag::All, 6);
    }
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);
    dialog.set_sizer_and_fit(sizer, true);
    dialog.centre();
    list.set_focus();

    let code = dialog.show_modal();
    dialog.destroy();
    let servers = servers.borrow().clone();
    let connect = if code == ID_CONNECT { *connect_index.borrow() } else { None };
    ManageResult { servers, connect }
}

/// The list entry for a server: "lan (10.0.0.2:6667, TLS, auto-connect)".
fn describe(server: &ServerConfig) -> String {
    let mut details = vec![format!("{}:{}", server.host, server.port)];
    if server.tls {
        details.push("TLS".into());
    }
    if server.auto_connect {
        details.push("auto-connect".into());
    }
    format!("{} ({})", server.name, details.join(", "))
}

fn save_or_complain(dialog: &Dialog, save: &dyn Fn(&[ServerConfig]) -> Result<(), String>, servers: &[ServerConfig]) {
    if let Err(e) = save(servers) {
        MessageDialog::builder(dialog, &format!("Couldn't save the server list:\n\n{e}"), "irctest")
            .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError)
            .build()
            .show_modal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_server() {
        let mut server = ServerConfig { name: "lan".into(), host: "10.0.0.2".into(), ..ServerConfig::default() };
        assert_eq!(describe(&server), "lan (10.0.0.2:6667)");
        server.tls = true;
        server.auto_connect = true;
        server.port = 6697;
        assert_eq!(describe(&server), "lan (10.0.0.2:6697, TLS, auto-connect)");
    }
}
