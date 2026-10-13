// No console window in release builds; debug builds keep it for println! output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

use wxdragon::prelude::*;

fn main() {
    // The manifest is embedded by build.rs; this just silences wx's check for it.
    SystemOptions::set_option_by_int("msw.no-manifest-check", 1);

    let _ = wxdragon::main(|_| {
        ui::main_frame::MainFrame::new().show();
    });
}
