#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_snapshot_does_not_infer_deletion_and_unknown_data_is_ignored() {
        let mut networks = Networks::default();
        networks
            .apply(
                &"BOUNCER NETWORK 42 name=First;unknown=opaque;pass=hidden"
                    .parse()
                    .unwrap(),
            )
            .unwrap();
        networks
            .apply(&"BOUNCER NETWORK 43 name=Second".parse().unwrap())
            .unwrap();
        networks.begin_snapshot();
        networks
            .apply(&"BOUNCER NETWORK 42 state=disconnected".parse().unwrap())
            .unwrap();
        assert!(networks.network("43").is_some());
        assert_eq!(networks.network("42").unwrap().attributes["name"], "First");
        assert!(
            !networks
                .network("42")
                .unwrap()
                .attributes
                .contains_key("unknown")
        );
        assert!(!format!("{networks:?}").contains("hidden"));
        assert_eq!(networks.finish_snapshot().len(), 1);
        assert!(networks.network("43").is_none());
        assert_eq!(
            networks.network("42").unwrap().attributes["state"],
            "disconnected"
        );
    }
    #[test]
    fn invalid_or_overlong_network_identifiers_are_rejected_without_echoing_input() {
        for id in ["*".to_string(), "x".repeat(501)] {
            let message: Message = Command::Raw(
                "BOUNCER".into(),
                vec!["NETWORK".into(), id, "pass=private".into()],
            )
            .into();
            let error = Networks::default().apply(&message).unwrap_err();
            assert!(!error.contains("private"));
        }
    }
    #[test]
    fn partial_updates_remove_attributes_and_never_retain_passwords() {
        let mut n = Networks::default();
        n.apply(
            &"BOUNCER NETWORK 42 name=My\\sNetwork;host=irc.test;pass=secret"
                .parse()
                .unwrap(),
        )
        .unwrap();
        n.apply(&"BOUNCER NETWORK 42 state=connected;host".parse().unwrap())
            .unwrap();
        assert_eq!(n.network("42").unwrap().attributes["name"], "My Network");
        assert!(!n.network("42").unwrap().attributes.contains_key("host"));
        assert!(!format!("{n:?}").contains("secret"));
        n.begin_snapshot();
        n.apply(&"BOUNCER NETWORK 43 name=Other".parse().unwrap())
            .unwrap();
        assert!(
            n.finish_snapshot()
                .iter()
                .any(|u| matches!(u, NetworkUpdate::Remove(id) if id == "42"))
        );
        n.apply(&"BOUNCER NETWORK 43 *".parse().unwrap()).unwrap();
        assert!(n.network("43").is_none());
    }
}

use irc_proto::{Command, Message};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub id: String,
    pub attributes: BTreeMap<String, String>,
}
impl Network {
    pub fn name(&self) -> &str {
        self.attributes
            .get("name")
            .or_else(|| self.attributes.get("host"))
            .map(String::as_str)
            .unwrap_or(&self.id)
    }
}
#[derive(Debug, Clone)]
pub enum NetworkUpdate {
    Upsert(Network),
    Remove(String),
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConnectionRole {
    #[default]
    Direct,
    Control,
    Bound(String),
}
#[derive(Debug, Default)]
pub struct Networks {
    records: BTreeMap<String, Network>,
    seen: Option<BTreeSet<String>>,
}
impl Networks {
    pub fn begin_snapshot(&mut self) {
        self.seen = Some(BTreeSet::new());
    }
    pub fn cancel_snapshot(&mut self) {
        self.seen = None;
    }
    pub fn network(&self, id: &str) -> Option<&Network> {
        self.records.get(id)
    }
    pub fn all(&self) -> impl Iterator<Item = &Network> {
        self.records.values()
    }
    pub fn apply(&mut self, message: &Message) -> Result<Option<NetworkUpdate>, String> {
        let Command::Raw(cmd, args) = &message.command else {
            return Ok(None);
        };
        if !cmd.eq_ignore_ascii_case("BOUNCER")
            || !args
                .first()
                .is_some_and(|s| s.eq_ignore_ascii_case("NETWORK"))
        {
            return Ok(None);
        }
        if args.len() != 3 || !valid_id(&args[1]) {
            return Err("Invalid bouncer network update.".into());
        }
        let id = &args[1];
        if let Some(seen) = &mut self.seen {
            seen.insert(id.clone());
        }
        if args[2] == "*" {
            self.records.remove(id);
            return Ok(Some(NetworkUpdate::Remove(id.clone())));
        }
        let record = self.records.entry(id.clone()).or_insert_with(|| Network {
            id: id.clone(),
            attributes: BTreeMap::new(),
        });
        for attr in args[2].split(';') {
            let (key, value) = attr
                .split_once('=')
                .map_or((attr, None), |(k, v)| (k, Some(v)));
            if ![
                "name", "host", "port", "tls", "state", "nickname", "username", "realname",
            ]
            .contains(&key)
            {
                continue;
            }
            if let Some(v) = value {
                record.attributes.insert(key.into(), unescape(v));
            } else {
                record.attributes.remove(key);
            }
        }
        Ok(Some(NetworkUpdate::Upsert(record.clone())))
    }
    pub fn finish_snapshot(&mut self) -> Vec<NetworkUpdate> {
        let Some(seen) = self.seen.take() else {
            return vec![];
        };
        let removed: Vec<_> = self
            .records
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect();
        removed
            .into_iter()
            .map(|id| {
                self.records.remove(&id);
                NetworkUpdate::Remove(id)
            })
            .collect()
    }
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 400
        && id != "*"
        && !id.starts_with(':')
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}
fn unescape(value: &str) -> String {
    let mut chars = value.chars();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(c) = chars.next() {
                result.push(match c {
                    's' => ' ',
                    ':' => ';',
                    'r' => '\r',
                    'n' => '\n',
                    c => c,
                });
            }
        } else {
            result.push(c);
        }
    }
    result
}
