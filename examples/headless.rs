//! Connects to a server from irctest.toml without the GUI, for testing the network layer.
//!
//!     cargo run --example headless [server name]
//!
//! Prints formatted incoming messages (and speech text), and sends each stdin line raw.
//! End of input sends QUIT and waits for the disconnect.

use std::io::BufRead;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use irctest::config::Config;
use irctest::format::format_message;
use irctest::net::{self, NetEvent};

fn main() {
    let config = Config::load().unwrap_or_else(|e| fail(&format!("Couldn't read config: {e}")));
    let server = match std::env::args().nth(1) {
        Some(name) => config.servers.iter().find(|s| s.name == name),
        None => config.servers.first(),
    }
    .unwrap_or_else(|| fail("No such server in irctest.toml"))
    .clone();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let sender: Arc<Mutex<Option<net::Sender>>> = Arc::default();
    let (done_tx, done_rx) = mpsc::channel();

    let sink_sender = sender.clone();
    let done_tx = Mutex::new(done_tx);
    let sink: net::EventSink = Arc::new(move |id, event| match event {
        NetEvent::Connected(s) => {
            println!("[{id}] connected");
            *sink_sender.lock().unwrap() = Some(s);
        }
        NetEvent::Message(msg) => match format_message(&msg) {
            Some(f) => match f.speech {
                Some(speech) => println!("[{id}] {}    [speech: {speech}]", f.text),
                None => println!("[{id}] {}", f.text),
            },
            None => {}
        },
        NetEvent::NickChanged(nick) => println!("[{id}] our nick is now {nick}"),
        NetEvent::Error(e) => println!("[{id}] error: {e}"),
        NetEvent::Disconnected { reason, .. } => {
            println!("[{id}] disconnected: {reason}");
            let _ = done_tx.lock().unwrap().send(());
        }
    });

    println!("Connecting to {}:{} as {}", server.host, server.port, config.effective_nick(&server));
    net::connect(runtime.handle(), 1, &config, &server, sink);

    // Disconnects can happen while we're blocked on stdin; check for one after each line.
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if done_rx.try_recv().is_ok() {
            return;
        }
        match sender.lock().unwrap().as_ref() {
            Some(s) => {
                if let Err(e) = net::send_raw(s, &line) {
                    println!("send failed: {e}");
                }
            }
            None => println!("not connected yet"),
        }
    }

    if let Some(s) = sender.lock().unwrap().as_ref() {
        let _ = s.send_quit("headless test done");
    }
    if done_rx.recv_timeout(Duration::from_secs(10)).is_err() {
        println!("no disconnect within 10 seconds");
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
