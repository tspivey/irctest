// No console window in release builds; debug builds keep it for println! output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

use irctest::config::{CONFIG_FILE, Config};
use wxdragon::prelude::*;

fn main() {
    // The manifest is embedded by build.rs; this just silences wx's check for it.
    SystemOptions::set_option_by_int("msw.no-manifest-check", 1);

    let _ = wxdragon::main(|_| match Config::load() {
        Ok(config) => ui::main_frame::MainFrame::new(&config).show(),
        Err(e) => {
            // Don't start with defaults: a later save would overwrite the user's file.
            let frame = Frame::builder().with_title("irctest").build();
            MessageDialog::builder(&frame, &format!("Couldn't read {CONFIG_FILE}:\n\n{e}"), "irctest")
                .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError)
                .build()
                .show_modal();
            frame.close(true);
        }
    });
}
