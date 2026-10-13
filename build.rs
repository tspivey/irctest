use embed_manifest::manifest::{ActiveCodePage, DpiAwareness, Setting, SupportedOS::*};
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("TARGET").unwrap_or_default().contains("windows") {
        // Common Controls v6 (themed, accessible controls), UTF-8 code page, per-monitor DPI.
        let manifest = new_manifest("irctest")
            .supported_os(Windows7..=Windows10)
            .active_code_page(ActiveCodePage::Utf8)
            .dpi_awareness(DpiAwareness::PerMonitorV2)
            .long_path_aware(Setting::Enabled);
        if let Err(e) = embed_manifest(manifest) {
            println!("cargo:warning=Failed to embed manifest: {e}");
        }
    }
}
