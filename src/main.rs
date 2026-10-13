// No console window in release builds; debug builds keep it for println! output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

use irctest::config::{CONFIG_FILE, Config};
use wxdragon::prelude::*;

fn main() {
    // The manifest is embedded by build.rs; this just silences wx's check for it.
    SystemOptions::set_option_by_int("msw.no-manifest-check", 1);

    // Network tasks run here; it lives until the GUI exits.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't start the tokio runtime");
    let handle = runtime.handle().clone();

    let _ = wxdragon::main(move |_| match Config::load() {
        Ok(config) => {
            let main = ui::main_frame::MainFrame::new(config, handle);
            main.show();
            main.connect_auto();
        }
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
