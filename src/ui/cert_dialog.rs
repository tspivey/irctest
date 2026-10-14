//! Asks what to do about a server certificate Windows doesn't trust.

use irctest::net::CertificatePrompt;
use wxdragon::prelude::*;

const ID_TRUST_ONCE: i32 = 3001;
const ID_TRUST_ALWAYS: i32 = 3002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateChoice {
    /// Connect, and trust this certificate again until irctest exits.
    TrustOnce,
    /// Connect, and save the fingerprint with the server.
    TrustAlways,
    Disconnect,
}

/// Shows the dialog for server `name`, whose saved fingerprint (blank if none) is `saved`.
/// Escape or closing the dialog disconnects.
pub fn ask_certificate(parent: &dyn WxWidget, name: &str, saved: &str, prompt: &CertificatePrompt) -> CertificateChoice {
    let dialog = Dialog::builder(parent, "Untrusted certificate")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    // The heading labels the details box, so a screen reader reads both on opening.
    let heading = if prompt.changed {
        format!("The certificate for {name} has &changed")
    } else {
        format!("The certificate for {name} isn't &trusted")
    };
    let label = StaticText::builder(&dialog).with_label(&heading).build();
    let details = TextCtrl::builder(&dialog)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
        .build();
    details.set_value(&details_text(saved, prompt));
    details.set_min_size(Size::new(480, 200));
    sizer.add(&label, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Top, 6);
    sizer.add(&details, 1, SizerFlag::Expand | SizerFlag::All, 6);

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let once = Button::builder(&dialog).with_id(ID_TRUST_ONCE).with_label("Trust &once").build();
    let always = Button::builder(&dialog).with_id(ID_TRUST_ALWAYS).with_label("Trust &always").build();
    let disconnect = Button::builder(&dialog).with_id(ID_CANCEL).with_label("&Disconnect").build();
    // Enter shouldn't trust anything by accident.
    disconnect.set_default();
    for button in [once, always, disconnect] {
        buttons.add(&button, 0, SizerFlag::All, 6);
    }
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);

    once.on_click(move |_| dialog.end_modal(ID_TRUST_ONCE));
    always.on_click(move |_| dialog.end_modal(ID_TRUST_ALWAYS));

    dialog.set_sizer_and_fit(sizer, true);
    dialog.centre();
    details.set_focus();
    let code = dialog.show_modal();
    dialog.destroy();
    match code {
        ID_TRUST_ONCE => CertificateChoice::TrustOnce,
        ID_TRUST_ALWAYS => CertificateChoice::TrustAlways,
        _ => CertificateChoice::Disconnect,
    }
}

fn details_text(saved: &str, prompt: &CertificatePrompt) -> String {
    let mut text = format!("Windows says: {}\n\nSHA-256 fingerprint:\n{}\n\n", prompt.reason, prompt.fingerprint);
    if prompt.changed {
        text.push_str(&format!(
            "The saved fingerprint is:\n{saved}\n\n\
             The server may have a new certificate, or someone may be intercepting the connection. \
             Check with the server's operator before trusting it.\n\n"
        ));
    } else {
        text.push_str(
            "Only trust it if you know this is the server's certificate: for example, you set up \
             the server, or the fingerprint matches one you got from its operator.\n\n",
        );
    }
    text.push_str(
        "Trust once: connect, and trust this certificate until irctest exits.\n\
         Trust always: connect, and save the fingerprint with this server.\n\
         Disconnect: don't connect.",
    );
    text
}
