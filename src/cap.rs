#[cfg(test)]
mod tests {
    use super::*;
    fn reply(n: &mut Negotiation, line: &str) -> Vec<CapAction> {
        n.handle_cap(&line.parse::<Message>().unwrap().command)
    }
    fn sent(actions: &[CapAction]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                CapAction::Send(m) => Some(m.to_string().trim().to_owned()),
                _ => None,
            })
            .collect()
    }
    #[test]
    fn no_sasl_multiline_and_partial_ack() {
        let mut n = Negotiation::new(CapOptions::default());
        assert_eq!(sent(&n.start()), ["CAP LS 302"]);
        assert!(reply(&mut n, "CAP * LS * :server-time echo-message").is_empty());
        let a = reply(&mut n, "CAP * LS :batch message-tags draft/chathistory");
        assert_eq!(sent(&a).len(), 1);
        assert!(
            !reply(&mut n, "CAP * ACK :server-time")
                .iter()
                .any(|a| matches!(a, CapAction::Ready))
        );
        let a = reply(
            &mut n,
            "CAP * ACK :batch message-tags echo-message draft/chathistory",
        );
        assert!(a.iter().any(|a| matches!(a, CapAction::Ready)));
        assert!(n.enabled().enabled("draft/chathistory"));
        assert!(
            !reply(&mut n, "CAP * ACK :server-time")
                .iter()
                .any(|a| matches!(a, CapAction::Ready))
        );
    }
    #[test]
    fn grouped_nak_retries_once_and_drops_dependencies() {
        let mut n = Negotiation::new(CapOptions::default());
        n.start();
        reply(
            &mut n,
            "CAP * LS :server-time batch message-tags draft/chathistory draft/read-marker",
        );
        assert_eq!(
            sent(&reply(
                &mut n,
                "CAP * NAK :server-time batch message-tags draft/chathistory draft/read-marker"
            ))
            .len(),
            5
        );
        reply(
            &mut n,
            "CAP * ACK :batch message-tags draft/chathistory draft/read-marker",
        );
        let a = reply(&mut n, "CAP * NAK :server-time");
        assert!(sent(&a).is_empty());
        assert!(!n.enabled().enabled("draft/chathistory"));
        assert!(!n.enabled().enabled("draft/read-marker"));
    }
    #[test]
    fn dynamic_caps_and_sasl_barrier() {
        let mut n = Negotiation::new(CapOptions {
            sasl_plain: true,
            ..Default::default()
        });
        n.start();
        reply(&mut n, "CAP * LS :sasl=PLAIN server-time");
        let a = reply(&mut n, "CAP * ACK :sasl server-time");
        assert!(a.iter().any(|a| matches!(a, CapAction::AuthenticatePlain)));
        assert!(!a.iter().any(|a| matches!(a, CapAction::Ready)));
        assert!(
            n.finish_sasl()
                .iter()
                .any(|a| matches!(a, CapAction::Ready))
        );
        assert!(n.finish_sasl().is_empty());
        assert_eq!(
            sent(&reply(&mut n, "CAP * NEW :echo-message")),
            ["CAP REQ echo-message"]
        );
        reply(&mut n, "CAP * ACK :echo-message");
        assert!(!local_echo(
            n.enabled(),
            &Command::PRIVMSG("bob".into(), "hi".into())
        ));
        reply(&mut n, "CAP * DEL :echo-message server-time");
        assert!(local_echo(
            n.enabled(),
            &Command::NOTICE("bob".into(), "hi".into())
        ));
    }
    #[test]
    fn partial_ack_never_exposes_a_capability_before_dependencies() {
        let mut n = Negotiation::new(CapOptions::default());
        reply(
            &mut n,
            "CAP * LS :draft/chathistory batch server-time message-tags echo-message",
        );
        reply(&mut n, "CAP * ACK :draft/chathistory batch message-tags");
        assert!(!n.enabled().enabled("draft/chathistory"));
        let a = reply(&mut n, "CAP * ACK :server-time");
        assert!(n.enabled().enabled("draft/chathistory"));
        assert!(!a.iter().any(|a| matches!(a, CapAction::Ready)));
        assert!(
            reply(&mut n, "CAP * ACK :echo-message")
                .iter()
                .any(|a| matches!(a, CapAction::Ready))
        );
    }
    #[test]
    fn new_during_listing_waits_for_last_ls() {
        let mut n = Negotiation::new(CapOptions::default());
        reply(&mut n, "CAP * LS * :server-time");
        assert!(reply(&mut n, "CAP * NEW :echo-message").is_empty());
        let a = reply(&mut n, "CAP * LS :batch");
        assert_eq!(sent(&a).len(), 1);
        assert!(sent(&a)[0].contains("echo-message"));
    }
    #[test]
    fn unrelated_new_does_not_retry_a_single_rejected_capability() {
        let mut n = Negotiation::new(CapOptions::default());
        reply(&mut n, "CAP * LS :server-time");
        reply(&mut n, "CAP * NAK :server-time");
        assert_eq!(
            sent(&reply(&mut n, "CAP * NEW :echo-message")),
            ["CAP REQ echo-message"]
        );
    }
    #[test]
    fn deleting_pending_dependency_releases_registration() {
        let mut n = Negotiation::new(CapOptions::default());
        reply(
            &mut n,
            "CAP * LS :draft/chathistory batch server-time message-tags",
        );
        reply(&mut n, "CAP * ACK :server-time message-tags");
        let a = reply(&mut n, "CAP * DEL :batch");
        assert!(a.iter().any(|a| matches!(a, CapAction::Ready)));
        assert!(!n.enabled().enabled("draft/chathistory"));
        assert!(reply(&mut n, "CAP * ACK :draft/chathistory").is_empty());
    }
    #[test]
    fn deleting_sasl_releases_authentication_barrier_once() {
        let mut n = Negotiation::new(CapOptions {
            sasl_plain: true,
            ..Default::default()
        });
        reply(&mut n, "CAP * LS :sasl=PLAIN");
        reply(&mut n, "CAP * ACK :sasl");
        assert!(
            reply(&mut n, "CAP * DEL :sasl")
                .iter()
                .any(|a| matches!(a, CapAction::Ready))
        );
        assert!(n.finish_sasl().is_empty());
        assert!(reply(&mut n, "CAP * DEL :sasl").is_empty());
    }
    #[test]
    fn sasl_new_after_registration_is_not_requested() {
        let mut n = Negotiation::new(CapOptions {
            sasl_plain: true,
            ..Default::default()
        });
        reply(&mut n, "CAP * LS :server-time");
        reply(&mut n, "CAP * ACK :server-time");
        assert!(reply(&mut n, "CAP * NEW :sasl=PLAIN").is_empty());
        n.abandon_registration();
        assert!(reply(&mut n, "CAP * NEW :sasl=PLAIN").is_empty());
    }
    #[test]
    fn unsupported_sasl_and_discovery_gates_are_respected() {
        let mut n = Negotiation::new(CapOptions {
            sasl_plain: true,
            discover_networks: true,
            ..Default::default()
        });
        let a = reply(
            &mut n,
            "CAP * LS :sasl=EXTERNAL soju.im/bouncer-networks soju.im/bouncer-networks-notify batch",
        );
        assert!(!sent(&a).join(" ").contains("sasl"));
        assert!(sent(&a).join(" ").contains("soju.im/bouncer-networks"));
        let mut n = Negotiation::new(CapOptions::default());
        let a = reply(
            &mut n,
            "CAP * LS :soju.im/bouncer-networks soju.im/bouncer-networks-notify batch",
        );
        assert!(!sent(&a).join(" ").contains("soju.im/"));
    }
    #[test]
    fn sasl_completion_still_waits_for_other_pending_capabilities() {
        let mut n = Negotiation::new(CapOptions {
            sasl_plain: true,
            ..Default::default()
        });
        reply(&mut n, "CAP * LS :sasl=EXTERNAL,PLAIN server-time");
        let actions = reply(&mut n, "CAP * ACK :sasl");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, CapAction::AuthenticatePlain))
        );
        assert!(n.finish_sasl().is_empty());
        assert!(
            reply(&mut n, "CAP * ACK :server-time")
                .iter()
                .any(|a| matches!(a, CapAction::Ready))
        );
        assert!(n.finish_sasl().is_empty());
    }
    #[test]
    fn new_dependencies_enable_history_and_del_disables_it() {
        let mut n = Negotiation::new(CapOptions::default());
        assert_eq!(
            sent(&reply(&mut n, "CAP * LS :draft/chathistory server-time")),
            ["CAP REQ server-time"]
        );
        reply(&mut n, "CAP * ACK :server-time");
        let actions = reply(&mut n, "CAP * NEW :batch message-tags");
        assert!(sent(&actions)[0].contains("draft/chathistory"));
        reply(&mut n, "CAP * ACK :batch draft/chathistory message-tags");
        assert!(n.enabled().enabled("draft/chathistory"));
        reply(&mut n, "CAP * DEL :batch");
        assert!(!n.enabled().enabled("draft/chathistory"));
        assert!(n.enabled().enabled("server-time"));
    }
    #[test]
    fn local_echo_all_chat_paths_and_unrelated_feedback() {
        let mut n = Negotiation::new(CapOptions::default());
        let commands = [
            "PRIVMSG #test :hello",
            "PRIVMSG bob :hello",
            "PRIVMSG bob :\u{1}ACTION waves\u{1}",
            "NOTICE bob :hello",
        ];
        for text in commands {
            assert!(local_echo(
                n.enabled(),
                &text.parse::<Message>().unwrap().command
            ));
        }
        reply(&mut n, "CAP * LS :echo-message");
        reply(&mut n, "CAP * ACK :echo-message");
        for text in commands {
            assert!(!local_echo(
                n.enabled(),
                &text.parse::<Message>().unwrap().command
            ));
        }
        assert!(local_echo(
            n.enabled(),
            &Command::JOIN("#test".into(), None, None)
        ));
    }
}

// Capability negotiation state, independent of transport and SASL payloads.
use irc_proto::{CapSubCommand, Command, Message};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities(BTreeSet<String>);
impl Capabilities {
    pub fn enabled(&self, name: &str) -> bool {
        self.0.contains(name)
    }
}
#[derive(Debug, Clone, Copy, Default)]
pub struct CapOptions {
    pub sasl_plain: bool,
    pub discover_networks: bool,
    pub bind_network: bool,
}
#[derive(Debug)]
pub enum CapAction {
    Send(Message),
    AuthenticatePlain,
    Ready,
    Changed(Capabilities),
    Note(String),
}
#[derive(Debug)]
pub struct Negotiation {
    options: CapOptions,
    offered: BTreeMap<String, String>,
    pending: BTreeSet<String>,
    retried: BTreeSet<String>,
    enabled: Capabilities,
    acknowledged: BTreeSet<String>,
    rejected: BTreeSet<String>,
    listing: bool,
    ready: bool,
    sasl_pending: bool,
}
const SUPPORTED: &[&str] = &[
    "sasl",
    "cap-notify",
    "server-time",
    "message-tags",
    "batch",
    "echo-message",
    "multi-prefix",
    "extended-join",
    "userhost-in-names",
    "draft/chathistory",
    "draft/read-marker",
    "soju.im/bouncer-networks",
    "soju.im/bouncer-networks-notify",
];
fn dependencies(name: &str) -> &'static [&'static str] {
    match name {
        "draft/chathistory" => &["batch", "server-time", "message-tags"],
        "draft/read-marker" => &["server-time"],
        "soju.im/bouncer-networks-notify" => &["soju.im/bouncer-networks", "batch"],
        _ => &[],
    }
}
impl Negotiation {
    pub fn new(options: CapOptions) -> Self {
        Self {
            options,
            offered: BTreeMap::new(),
            pending: BTreeSet::new(),
            retried: BTreeSet::new(),
            enabled: Capabilities::default(),
            acknowledged: BTreeSet::new(),
            rejected: BTreeSet::new(),
            listing: true,
            ready: false,
            sasl_pending: false,
        }
    }
    pub fn start(&mut self) -> Vec<CapAction> {
        vec![CapAction::Send(
            Command::CAP(None, CapSubCommand::LS, Some("302".into()), None).into(),
        )]
    }
    pub fn enabled(&self) -> &Capabilities {
        &self.enabled
    }
    pub fn unfinished(&self) -> bool {
        !self.ready
    }
    pub fn abandon_registration(&mut self) {
        self.ready = true;
        self.listing = false;
        self.pending.clear();
        self.sasl_pending = false;
    }
    fn eligible(&self, name: &str) -> bool {
        self.offered.contains_key(name)
            && SUPPORTED.contains(&name)
            && !self.rejected.contains(name)
            && (name != "sasl"
                || (!self.ready
                    && self.options.sasl_plain
                    && self.offered.get(name).is_some_and(|m| {
                        m.is_empty() || m.split(',').any(|v| v.eq_ignore_ascii_case("PLAIN"))
                    })))
            && (!name.starts_with("soju.im/")
                || self.options.bind_network
                || (self.options.discover_networks && self.options.sasl_plain))
            && (name != "soju.im/bouncer-networks-notify" || !self.options.bind_network)
            && dependencies(name).iter().all(|d| self.eligible(d))
    }
    fn request(&mut self, names: Vec<String>, individual: bool) -> Vec<CapAction> {
        let mut actions = Vec::new();
        let mut line = String::new();
        for name in names {
            if !line.is_empty() && (individual || line.len() + name.len() + 1 > 490) {
                actions.push(CapAction::Send(
                    Command::CAP(
                        None,
                        CapSubCommand::REQ,
                        None,
                        Some(std::mem::take(&mut line)),
                    )
                    .into(),
                ));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&name);
            self.pending.insert(name);
        }
        if !line.is_empty() {
            actions.push(CapAction::Send(
                Command::CAP(None, CapSubCommand::REQ, None, Some(line)).into(),
            ));
        }
        actions
    }
    fn settle(&mut self, actions: &mut Vec<CapAction>) {
        // Keep raw ACKs while dependencies are still pending, but expose only
        // capabilities whose complete dependency chain has been acknowledged.
        self.enabled.0 = self.acknowledged.clone();
        loop {
            let invalid: Vec<_> = self
                .enabled
                .0
                .iter()
                .filter(|n| dependencies(n).iter().any(|d| !self.enabled.enabled(d)))
                .cloned()
                .collect();
            if invalid.is_empty() {
                break;
            }
            for name in invalid {
                self.enabled.0.remove(&name);
            }
        }
        if !self.listing && self.pending.is_empty() && !self.ready && !self.sasl_pending {
            self.ready = true;
            actions.push(CapAction::Ready);
        }
    }
    pub fn finish_sasl(&mut self) -> Vec<CapAction> {
        if !self.sasl_pending {
            return vec![];
        }
        self.sasl_pending = false;
        let mut actions = vec![];
        self.settle(&mut actions);
        actions
    }
    pub fn handle_cap(&mut self, command: &Command) -> Vec<CapAction> {
        let Command::CAP(_, sub, third, fourth) = command else {
            return vec![];
        };
        let more = third.as_deref() == Some("*") && fourth.is_some();
        let caps = fourth.as_deref().or(third.as_deref()).unwrap_or("");
        let old = self.enabled.clone();
        let mut actions = Vec::new();
        match sub {
            CapSubCommand::LS | CapSubCommand::NEW => {
                if matches!(sub, CapSubCommand::LS) && !self.listing {
                    return vec![];
                }
                for cap in caps.split_whitespace() {
                    let (name, value) = cap.split_once('=').unwrap_or((cap, ""));
                    self.offered.insert(name.into(), value.into());
                }
                if matches!(sub, CapSubCommand::LS) {
                    if !self.listing || more {
                        return vec![];
                    }
                    self.listing = false;
                    if self.options.sasl_plain && !self.eligible("sasl") {
                        actions.push(CapAction::Note("The server doesn't support SASL PLAIN; carrying on without logging in.".into()));
                    }
                }
                if self.listing {
                    return vec![];
                }
                let names = SUPPORTED
                    .iter()
                    .filter(|n| {
                        self.eligible(n)
                            && !self.pending.contains(**n)
                            && !self.acknowledged.contains(**n)
                    })
                    .map(|n| n.to_string())
                    .collect();
                actions.extend(self.request(names, false));
            }
            CapSubCommand::ACK => {
                for token in caps.split_whitespace() {
                    let name = token
                        .trim_start_matches('-')
                        .split('=')
                        .next()
                        .unwrap_or("");
                    if !self.pending.remove(name) {
                        continue;
                    }
                    if token.starts_with('-') {
                        self.acknowledged.remove(name);
                        self.rejected.insert(name.into());
                    } else {
                        self.acknowledged.insert(name.into());
                        if name == "sasl" && !self.ready {
                            self.sasl_pending = true;
                            actions.push(CapAction::AuthenticatePlain);
                        }
                    }
                }
            }
            CapSubCommand::NAK => {
                let names: Vec<_> = caps
                    .split_whitespace()
                    .map(|s| {
                        s.trim_start_matches('-')
                            .split('=')
                            .next()
                            .unwrap_or("")
                            .to_owned()
                    })
                    .filter(|n| self.pending.remove(n))
                    .collect();
                let retry: Vec<_> = if names.len() > 1 {
                    names
                        .iter()
                        .filter(|n| self.eligible(n) && self.retried.insert((*n).clone()))
                        .cloned()
                        .collect()
                } else {
                    vec![]
                };
                for name in &names {
                    if !retry.contains(name) {
                        self.rejected.insert(name.clone());
                    }
                }
                if names.iter().any(|n| n == "sasl") && retry.is_empty() {
                    actions.push(CapAction::Note(
                        "The server refused SASL; carrying on without logging in.".into(),
                    ));
                }
                actions.extend(self.request(retry, true));
            }
            CapSubCommand::DEL => {
                for token in caps.split_whitespace() {
                    let name = token.split('=').next().unwrap_or(token);
                    self.offered.remove(name);
                    self.pending.remove(name);
                    self.acknowledged.remove(name);
                    self.rejected.remove(name);
                    self.retried.remove(name);
                    if name == "sasl" {
                        self.sasl_pending = false;
                    }
                }
                let invalid: Vec<_> = self
                    .pending
                    .iter()
                    .chain(self.acknowledged.iter())
                    .filter(|n| dependencies(n).iter().any(|d| !self.eligible(d)))
                    .cloned()
                    .collect();
                for name in invalid {
                    self.pending.remove(&name);
                    self.acknowledged.remove(&name);
                }
            }
            _ => {}
        }
        self.settle(&mut actions);
        if old != self.enabled {
            actions.insert(0, CapAction::Changed(self.enabled.clone()));
        }
        actions
    }
}
pub fn local_echo(enabled: &Capabilities, command: &Command) -> bool {
    !matches!(command, Command::PRIVMSG(..) | Command::NOTICE(..))
        || !enabled.enabled("echo-message")
}
