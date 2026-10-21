use crate::config::{Config, ServerConfig};
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant};
pub const STATE_FILE: &str = "irctest-state.toml";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountKey {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub identity: String,
}
impl AccountKey {
    pub fn from_config(config: &Config, server: &ServerConfig) -> Self {
        let identity = if server.sasl_password.is_empty() {
            encode(&["user", config.effective_username(server)])
        } else {
            let sasl_identity = match server.sasl_username.trim() {
                "" => config.effective_nick(server),
                username => username,
            };
            encode(&["sasl", sasl_identity, config.effective_username(server)])
        };
        Self {
            name: server.name.clone(),
            host: server.host.trim().to_ascii_lowercase(),
            port: server.port,
            tls: server.tls,
            identity,
        }
    }
    pub fn canonical(&self) -> String {
        encode(&[
            &self.name,
            &self.host,
            &self.port.to_string(),
            if self.tls { "tls" } else { "plain" },
            &self.identity,
        ])
    }
}
fn encode(parts: &[&str]) -> String {
    parts.iter().map(|p| format!("{}:{p}", p.len())).collect()
}
#[derive(Debug, Clone)]
pub struct ConversationKey {
    pub account: AccountKey,
    pub network_id: Option<String>,
    pub target: String,
}
impl ConversationKey {
    pub fn canonical(&self) -> String {
        encode(&[
            &self.account.canonical(),
            if self.network_id.is_some() {
                "network"
            } else {
                "direct"
            },
            self.network_id.as_deref().unwrap_or(""),
            &crate::route::irc_lower(&self.target),
        ])
    }
}
/// Where a conversation's speech setting is kept: the configured server name, the
/// bouncer network if any, and the conversation. Unlike `ConversationKey` it leaves out the
/// endpoint and login, so changing the nick, username or address keeps the setting.
pub fn speech_key(server: &str, network_id: Option<&str>, target: &str) -> String {
    encode(&[
        server,
        network_id.unwrap_or(""),
        &crate::route::irc_lower(target),
    ])
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub time: String,
    pub msgid: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationState {
    pub received: Option<Cursor>,
    pub read: Option<String>,
    /// For deduplicating a tab opened again after a reconnect. Kept in memory only: a
    /// restart starts every tab empty and doesn't use them.
    #[serde(skip)]
    pub recent_ids: Vec<String>,
}
pub fn timestamp(value: &str) -> Result<DateTime<FixedOffset>, String> {
    DateTime::parse_from_rfc3339(value).map_err(|_| "Invalid server timestamp.".into())
}
impl ConversationState {
    pub fn mark_read(&mut self, value: &str) -> Result<bool, String> {
        let time = timestamp(value)?;
        if self
            .read
            .as_deref()
            .and_then(|s| timestamp(s).ok())
            .is_some_and(|old| old >= time)
        {
            return Ok(false);
        }
        self.read = Some(value.into());
        Ok(true)
    }
    pub fn observe(&mut self, cursor: Cursor) {
        let Ok(new) = timestamp(&cursor.time) else {
            return;
        };
        if self
            .received
            .as_ref()
            .and_then(|c| timestamp(&c.time).ok())
            .is_none_or(|old| new >= old)
        {
            self.received = Some(cursor);
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncData {
    pub version: u32,
    #[serde(default)]
    pub conversations: BTreeMap<String, ConversationState>,
    /// `speech_key`s of conversations F5 turned speech off for.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub speech_off: BTreeSet<String>,
}
impl Default for SyncData {
    fn default() -> Self {
        Self {
            version: 1,
            conversations: BTreeMap::new(),
            speech_off: BTreeSet::new(),
        }
    }
}
/// Conversations with nothing received or read for this long are forgotten.
pub const STALE_AFTER: chrono::Duration = chrono::Duration::days(30);
impl SyncData {
    /// Drops conversations whose newest received message and read marker are both older
    /// than `STALE_AFTER`. Speech settings are kept.
    pub fn prune_stale(&mut self, now: DateTime<Utc>) {
        let cutoff = now - STALE_AFTER;
        self.conversations.retain(|_, state| {
            let received = state.received.as_ref().map(|c| c.time.as_str());
            [received, state.read.as_deref()]
                .into_iter()
                .flatten()
                .filter_map(|time| timestamp(time).ok())
                .any(|time| time >= cutoff)
        });
    }
}
pub fn load(path: &Path) -> Result<SyncData, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SyncData::default()),
        Err(e) => return Err(e.to_string()),
    };
    let mut data: SyncData = toml::from_str(&text)
        .map_err(|_| "Invalid synchronization state file; using memory only.".to_string())?;
    validate(&mut data)?;
    Ok(data)
}
fn validate(data: &mut SyncData) -> Result<(), String> {
    if data.version != 1 {
        return Err("Unsupported synchronization state version; using memory only.".into());
    }
    for state in data.conversations.values_mut() {
        if let Some(c) = &state.received {
            timestamp(&c.time)?;
        }
        if let Some(t) = &state.read {
            timestamp(t)?;
        }
    }
    Ok(())
}
pub fn save(path: &Path, data: &SyncData) -> Result<(), String> {
    // A corrupt or newer file must remain available for recovery.
    load(path)?;
    let mut data = data.clone();
    validate(&mut data)?;
    let text = toml::to_string(&data).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}
enum WriteCommand {
    Update(SyncData),
    Flush(oneshot::Sender<Result<(), String>>),
}
#[derive(Clone)]
pub struct Writer {
    tx: mpsc::UnboundedSender<WriteCommand>,
    error: Arc<Mutex<Option<String>>>,
}
impl Writer {
    pub fn spawn(handle: &Handle, path: PathBuf, writable: bool) -> Self {
        Self::spawn_with_save(handle, path, writable, save).0
    }
    fn spawn_with_save<F>(
        handle: &Handle,
        path: PathBuf,
        writable: bool,
        save: F,
    ) -> (Self, tokio::task::JoinHandle<()>)
    where
        F: Fn(&Path, &SyncData) -> Result<(), String> + Send + Sync + 'static,
    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let error = Arc::new(Mutex::new(None));
        let report = error.clone();
        let save = Arc::new(save);
        let task = handle.spawn(async move {
            let mut latest: Option<SyncData> = None;
            let mut due: Option<Instant> = None;
            let mut reported = false;
            loop {
                let timer = async { match due { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } };
                let mut flush = None;
                let mut closing = false;
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(WriteCommand::Update(data)) => { latest = Some(data); due.get_or_insert(Instant::now() + Duration::from_secs(2)); continue; }
                        Some(WriteCommand::Flush(ack)) => flush = Some(ack),
                        None => closing = true,
                    },
                    _ = timer => {}
                }
                due = None;
                let result = if writable {
                    if let Some(data) = latest.clone() {
                        let path = path.clone();
                        let save = save.clone();
                        tokio::task::spawn_blocking(move || save(&path, &data)).await.unwrap_or_else(|e| Err(e.to_string()))
                    } else { Ok(()) }
                } else { Ok(()) };
                if result.is_ok() { latest = None; }
                else {
                    // Retain the dirty snapshot for a later flush or debounce retry.
                    due = Some(Instant::now() + Duration::from_secs(2));
                    if !reported {
                        *report.lock().unwrap() = result.as_ref().err().cloned();
                        reported = true;
                    }
                }
                if let Some(ack) = flush { let _ = ack.send(result); }
                if closing { break; }
            }
        });
        (Self { tx, error }, task)
    }
    pub fn update(&self, data: SyncData) {
        let _ = self.tx.send(WriteCommand::Update(data));
    }
    pub async fn flush(&self) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Flush(tx))
            .map_err(|_| "State writer closed.".to_string())?;
        rx.await.map_err(|_| "State writer closed.".to_string())?
    }
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_isolation_and_monotonic_instants() {
        let config = Config {
            nickname: "me".into(),
            ..Default::default()
        };
        let mut server = ServerConfig {
            name: "soju".into(),
            host: "bouncer.test".into(),
            sasl_username: "account".into(),
            sasl_password: "initial-secret".into(),
            ..Default::default()
        };
        let first = AccountKey::from_config(&config, &server);
        server.sasl_password = "secret".into();
        assert_eq!(
            first.canonical(),
            AccountKey::from_config(&config, &server).canonical()
        );
        server.sasl_username = "other".into();
        assert_ne!(
            first.canonical(),
            AccountKey::from_config(&config, &server).canonical()
        );
        let mut s = ConversationState::default();
        assert!(s.mark_read("2026-10-20T10:00:00Z").unwrap());
        assert!(!s.mark_read("2026-10-20T11:00:00+01:00").unwrap());
        assert!(!s.mark_read("2026-10-20T09:00:00Z").unwrap());
        assert!(s.mark_read("*").is_err());
    }
    #[test]
    fn sasl_default_identity_tracks_the_actual_authentication_nickname() {
        let mut config = Config {
            nickname: "first-account".into(),
            username: "fixed-user".into(),
            ..Default::default()
        };
        let server = ServerConfig {
            sasl_password: "secret".into(),
            ..Default::default()
        };
        let first = AccountKey::from_config(&config, &server).canonical();
        config.nickname = "second-account".into();
        assert_ne!(first, AccountKey::from_config(&config, &server).canonical());
        assert!(!first.contains("secret"));
    }
    #[test]
    fn unused_sasl_username_does_not_change_plain_user_identity() {
        let config = Config {
            username: "fixed-user".into(),
            ..Default::default()
        };
        let mut server = ServerConfig::default();
        let user = AccountKey::from_config(&config, &server).canonical();
        server.sasl_username = "unused-account".into();
        assert_eq!(user, AccountKey::from_config(&config, &server).canonical());
        server.sasl_password = "secret".into();
        assert_ne!(user, AccountKey::from_config(&config, &server).canonical());
    }
    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "irc-sync-{}-{}-{label}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn snapshot(name: &str) -> SyncData {
        let mut data = SyncData::default();
        data.conversations
            .insert(name.into(), ConversationState::default());
        data
    }
    #[test]
    fn prune_stale_drops_old_conversations_and_keeps_speech() {
        let now = timestamp("2026-10-21T12:00:00Z").unwrap().to_utc();
        let cursor = |time: &str| Cursor {
            time: time.into(),
            msgid: None,
        };
        let mut data = SyncData::default();
        let mut state = |received: Option<&str>, read: Option<&str>| ConversationState {
            received: received.map(cursor),
            read: read.map(Into::into),
            recent_ids: Vec::new(),
        };
        let recent = state(Some("2026-10-20T00:00:00Z"), None);
        let old = state(Some("2026-08-01T00:00:00Z"), Some("2026-08-01T00:00:00Z"));
        let read_recently = state(Some("2026-08-01T00:00:00Z"), Some("2026-10-01T00:00:00Z"));
        let empty = state(None, None);
        data.conversations.insert("recent".into(), recent);
        data.conversations.insert("old".into(), old);
        data.conversations.insert("read".into(), read_recently);
        data.conversations.insert("empty".into(), empty);
        data.speech_off.insert(speech_key("Net", None, "#old"));
        data.prune_stale(now);
        let kept: Vec<_> = data.conversations.keys().map(String::as_str).collect();
        assert_eq!(kept, ["read", "recent"]);
        assert_eq!(data.speech_off.len(), 1);
    }
    #[test]
    fn speech_off_survives_save_and_load() {
        let path = temp_path("speech");
        let mut data = SyncData::default();
        data.speech_off
            .insert(speech_key("Bouncer", Some("7"), "#Quiet"));
        save(&path, &data).unwrap();
        let loaded = load(&path).unwrap();
        assert!(
            loaded
                .speech_off
                .contains(&speech_key("Bouncer", Some("7"), "#quiet"))
        );
        assert!(
            !loaded
                .speech_off
                .contains(&speech_key("Bouncer", None, "#quiet"))
        );
        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn conversation_namespaces_isolate_endpoint_accounts_and_opaque_networks() {
        let config = Config {
            username: "account".into(),
            ..Default::default()
        };
        let server = ServerConfig {
            name: "home".into(),
            host: " Example.Test ".into(),
            port: 6697,
            tls: true,
            ..Default::default()
        };
        let account = AccountKey::from_config(&config, &server);
        assert_eq!(account.host, "example.test");
        let key = ConversationKey {
            account: account.clone(),
            network_id: Some("1".into()),
            target: "[Chat]".into(),
        };
        let canonical = key.canonical();
        let folded = ConversationKey {
            target: "{chat}".into(),
            ..key.clone()
        };
        assert_eq!(canonical, folded.canonical());
        assert_ne!(
            canonical,
            ConversationKey {
                network_id: Some("2".into()),
                ..key.clone()
            }
            .canonical()
        );
        assert_ne!(
            canonical,
            ConversationKey {
                network_id: None,
                ..key.clone()
            }
            .canonical()
        );
        for changed in [
            AccountKey {
                name: "other".into(),
                ..account.clone()
            },
            AccountKey {
                host: "elsewhere".into(),
                ..account.clone()
            },
            AccountKey {
                port: 6667,
                ..account.clone()
            },
            AccountKey {
                tls: false,
                ..account.clone()
            },
            AccountKey {
                identity: "other".into(),
                ..account.clone()
            },
        ] {
            assert_ne!(
                canonical,
                ConversationKey {
                    account: changed,
                    ..key.clone()
                }
                .canonical()
            );
        }
    }
    #[test]
    fn pass_only_identity_uses_manual_username_without_secret() {
        let mut config = Config {
            nickname: "same-nick".into(),
            username: "account/first".into(),
            ..Default::default()
        };
        let server = ServerConfig {
            password: "private-pass".into(),
            ..Default::default()
        };
        let first = AccountKey::from_config(&config, &server).canonical();
        config.username = "account/second".into();
        assert_ne!(first, AccountKey::from_config(&config, &server).canonical());
        assert!(!first.contains("private-pass"));
    }
    #[test]
    fn save_rejects_invalid_data_and_keeps_recent_ids_in_memory() {
        let path = temp_path("validation");
        let mut data = snapshot("chat");
        data.conversations.get_mut("chat").unwrap().recent_ids = vec!["saved-id".into()];
        save(&path, &data).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("recent_ids") && !text.contains("saved-id"));
        let original = std::fs::read(&path).unwrap();
        data.conversations.get_mut("chat").unwrap().read = Some("invalid".into());
        assert!(save(&path, &data).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn failed_save_keeps_dirty_snapshot_for_retry() {
        let root = temp_path("retry-dir");
        let path = root.join("state.toml");
        let writer = Writer::spawn(&Handle::current(), path.clone(), true);
        writer.update(snapshot("retained"));
        assert!(writer.flush().await.is_err());
        assert!(writer.take_error().is_some());
        assert!(writer.flush().await.is_err());
        assert!(
            writer.take_error().is_none(),
            "only the first save error is reported"
        );
        std::fs::create_dir(&root).unwrap();
        writer.flush().await.unwrap();
        assert!(load(&path).unwrap().conversations.contains_key("retained"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn debounce_coalesces_for_two_seconds_and_drop_saves() {
        let (writes, mut received) = mpsc::unbounded_channel();
        let (writer, task) =
            Writer::spawn_with_save(&Handle::current(), PathBuf::new(), true, move |_, data| {
                writes
                    .send(data.conversations.keys().cloned().collect::<Vec<_>>())
                    .unwrap();
                Ok(())
            });
        writer.update(snapshot("first"));
        writer.update(snapshot("second"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(received.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(received.recv().await.unwrap(), vec!["second"]);
        writer.update(snapshot("last"));
        drop(writer);
        task.await.unwrap();
        assert_eq!(received.recv().await.unwrap(), vec!["last"]);
        assert!(received.try_recv().is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn update_during_inflight_write_survives_flush_and_shutdown() {
        let path = temp_path("inflight");
        let (started, ready) = oneshot::channel();
        let started = Mutex::new(Some(started));
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Mutex::new(blocked);
        let (writer, task) =
            Writer::spawn_with_save(&Handle::current(), path.clone(), true, move |path, data| {
                if let Some(started) = started.lock().unwrap().take() {
                    started.send(()).unwrap();
                    blocked.lock().unwrap().recv().unwrap();
                }
                save(path, data)
            });
        writer.update(snapshot("first"));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        ready.await.unwrap();
        writer.update(snapshot("latest"));
        let flush_writer = writer.clone();
        let flush = tokio::spawn(async move { flush_writer.flush().await });
        release.send(()).unwrap();
        flush.await.unwrap().unwrap();
        assert!(load(&path).unwrap().conversations.contains_key("latest"));
        writer.update(snapshot("shutdown"));
        drop(writer);
        task.await.unwrap();
        assert!(load(&path).unwrap().conversations.contains_key("shutdown"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn invalid_existing_file_and_unsupported_output_stay_untouched() {
        let path = temp_path("invalid-preserved");
        for original in ["invalid = [", "version = 999\n"] {
            std::fs::write(&path, original).unwrap();
            assert!(save(&path, &snapshot("new")).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        std::fs::remove_file(&path).unwrap();
        let mut future = SyncData::default();
        future.version = 999;
        assert!(save(&path, &future).is_err());
        assert!(!path.exists());
    }
    #[tokio::test(start_paused = true)]
    async fn serialized_flush_keeps_latest_and_readonly_preserves_invalid_file() {
        let path = std::env::temp_dir().join(format!("irc-sync-{}.toml", std::process::id()));
        let writer = Writer::spawn(&Handle::current(), path.clone(), true);
        let mut data = SyncData::default();
        data.conversations
            .insert("first".into(), ConversationState::default());
        writer.update(data.clone());
        data.conversations
            .insert("second".into(), ConversationState::default());
        writer.update(data.clone());
        writer.flush().await.unwrap();
        assert_eq!(load(&path).unwrap().conversations.len(), 2);
        std::fs::write(&path, "version = 999\n").unwrap();
        assert!(load(&path).is_err());
        let writer = Writer::spawn(&Handle::current(), path.clone(), false);
        writer.update(data);
        writer.flush().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version = 999\n");
        std::fs::remove_file(path).unwrap();
    }
}
