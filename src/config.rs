//! Settings stored in `irctest.toml` in the current directory.

use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "irctest.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, from = "StoredConfig")]
pub struct Config {
    /// Used for every server whose own nickname is blank.
    pub nickname: String,
    /// Blank = use the nickname.
    pub username: String,
    /// Blank = use the nickname.
    pub realname: String,
    /// Write conversations to `logs/<server>/<target>/YYYY-MM-DD.log`.
    pub log: bool,
    pub servers: Vec<ServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            username: String::new(),
            realname: String::new(),
            log: true,
            servers: Vec::new(),
        }
    }
}

// Read the former global list once and preserve it on each existing server.
// New servers never inherit ignores, and saves only write server-specific lists.
#[derive(Deserialize)]
#[serde(default)]
struct StoredConfig {
    nickname: String,
    username: String,
    realname: String,
    log: bool,
    ignored: Vec<String>,
    servers: Vec<ServerConfig>,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            username: String::new(),
            realname: String::new(),
            log: true,
            ignored: Vec::new(),
            servers: Vec::new(),
        }
    }
}

impl From<StoredConfig> for Config {
    fn from(mut stored: StoredConfig) -> Self {
        for server in &mut stored.servers {
            for name in &stored.ignored {
                if !crate::ignore::contains(&server.ignored, name) {
                    server.ignored.push(name.clone());
                }
            }
        }
        Self {
            nickname: stored.nickname,
            username: stored.username,
            realname: stored.realname,
            log: stored.log,
            servers: stored.servers,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Exact nicknames and channel names to hide on this configured server.
    pub ignored: Vec<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// Sent as PASS when not blank. For ZNC: "user/network:pass".
    pub password: String,
    /// Blank = use the global nickname.
    pub nickname: String,
    pub auto_connect: bool,
    pub discover_networks: bool,
    /// Raw IRC lines sent after registration (001).
    pub connect_commands: Vec<String>,
    /// SHA-256 fingerprint of a TLS certificate to accept even though Windows doesn't trust it
    /// (e.g. self-signed), as colon-separated hex. Blank = none.
    pub fingerprint: String,
    /// Account to log in to with SASL PLAIN. Blank = the nickname.
    pub sasl_username: String,
    /// SASL is used when this isn't blank.
    pub sasl_password: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            ignored: Vec::new(),
            name: String::new(),
            host: String::new(),
            port: 6667,
            tls: false,
            password: String::new(),
            nickname: String::new(),
            auto_connect: false,
            discover_networks: true,
            connect_commands: Vec::new(),
            fingerprint: String::new(),
            sasl_username: String::new(),
            sasl_password: String::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "{e}"),
            ConfigError::Parse(e) => write!(f, "{e}"),
            ConfigError::Serialize(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Loads `irctest.toml` from the current directory.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(Path::new(CONFIG_FILE))
    }

    /// A missing file gives the default (empty) config; any other failure is an error,
    /// so a broken file is never silently replaced by defaults.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(ConfigError::Parse),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(Path::new(CONFIG_FILE))
    }

    /// Writes to a temporary file first and renames it over the old one,
    /// so a failed write can't leave a half-written config behind.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(ConfigError::Io)?;
        std::fs::rename(&tmp, path).map_err(ConfigError::Io)
    }

    /// The server's nickname, or the global one if the server's is blank.
    /// Empty if neither is set.
    pub fn effective_nick<'a>(&'a self, server: &'a ServerConfig) -> &'a str {
        let nick = server.nickname.trim();
        if nick.is_empty() {
            self.nickname.trim()
        } else {
            nick
        }
    }

    pub fn effective_username<'a>(&'a self, server: &'a ServerConfig) -> &'a str {
        let user = self.username.trim();
        if user.is_empty() {
            self.effective_nick(server)
        } else {
            user
        }
    }

    pub fn effective_realname<'a>(&'a self, server: &'a ServerConfig) -> &'a str {
        let real = self.realname.trim();
        if real.is_empty() {
            self.effective_nick(server)
        } else {
            real
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A path in the system temp dir that is removed when dropped.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("irctest-{}-{name}.toml", std::process::id()));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn sample() -> Config {
        Config {
            nickname: "tyler".into(),
            username: String::new(),
            realname: "Tyler".into(),
            log: false,
            servers: vec![ServerConfig {
                ignored: Vec::new(),
                name: "lan".into(),
                host: "10.0.0.2".into(),
                port: 6667,
                tls: false,
                password: "user/net:pass".into(),
                nickname: String::new(),
                auto_connect: true,
                discover_networks: true,
                connect_commands: vec!["JOIN #test".into(), "PRIVMSG NickServ :IDENTIFY x".into()],
                fingerprint: "AB:CD".into(),
                sasl_username: "account".into(),
                sasl_password: "secret".into(),
            }],
        }
    }

    #[test]
    fn missing_file_gives_default() {
        let file = TempFile::new("missing");
        assert_eq!(Config::load_from(&file.0).unwrap(), Config::default());
    }

    #[test]
    fn discovery_defaults_on_and_can_be_disabled_in_old_compatible_config() {
        let old: Config =
            toml::from_str("nickname = 'me'\n[[servers]]\nhost = 'bouncer.test'\n").unwrap();
        assert!(old.servers[0].discover_networks);
        let disabled: Config = toml::from_str(
            "nickname = 'me'\n[[servers]]\nhost = 'bouncer.test'\ndiscover_networks = false\n",
        )
        .unwrap();
        assert!(!disabled.servers[0].discover_networks);
        let serialized = toml::to_string(&disabled).unwrap();
        assert_eq!(toml::from_str::<Config>(&serialized).unwrap(), disabled);
    }
    #[test]
    fn round_trip() {
        let file = TempFile::new("roundtrip");
        let config = sample();
        config.save_to(&file.0).unwrap();
        assert_eq!(Config::load_from(&file.0).unwrap(), config);
    }

    #[test]
    fn ignore_lists_are_saved_separately_for_each_server() {
        let file = TempFile::new("ignored");
        let config: Config = toml::from_str(
            "[[servers]]\nname = 'one'\nignored = ['Bob', '#quiet']\n\n[[servers]]\nname = 'two'\nignored = ['Alice']\n",
        ).unwrap();
        config.save_to(&file.0).unwrap();
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(&file.0).unwrap()).unwrap();
        assert!(saved.get("ignored").is_none(), "ignores must not be global");
        assert_eq!(
            saved["servers"][0]["ignored"].as_array().unwrap(),
            &vec![
                toml::Value::String("Bob".into()),
                toml::Value::String("#quiet".into())
            ]
        );
        assert_eq!(
            saved["servers"][1]["ignored"].as_array().unwrap(),
            &vec![toml::Value::String("Alice".into())]
        );
        assert_eq!(Config::load_from(&file.0).unwrap(), config);
    }

    #[test]
    fn legacy_global_ignores_migrate_to_existing_servers_only() {
        let file = TempFile::new("legacy-ignored");
        std::fs::write(&file.0,
            "ignored = ['Bob', '#quiet']\n[[servers]]\nname = 'one'\nignored = ['bOB', 'Alice']\n[[servers]]\nname = 'two'\n"
        ).unwrap();
        let config = Config::load_from(&file.0).unwrap();
        config.save_to(&file.0).unwrap();
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(&file.0).unwrap()).unwrap();
        assert!(saved.get("ignored").is_none());
        assert_eq!(
            saved["servers"][0]["ignored"].as_array().unwrap(),
            &vec![
                toml::Value::String("bOB".into()),
                toml::Value::String("Alice".into()),
                toml::Value::String("#quiet".into())
            ]
        );
        assert_eq!(
            saved["servers"][1]["ignored"].as_array().unwrap(),
            &vec![
                toml::Value::String("Bob".into()),
                toml::Value::String("#quiet".into())
            ]
        );
    }

    #[test]
    fn missing_fields_use_defaults() {
        let config: Config = toml::from_str(
            r#"
            nickname = "tyler"

            [[servers]]
            name = "lan"
            host = "10.0.0.2"
            "#,
        )
        .unwrap();
        let server = &config.servers[0];
        assert_eq!(server.port, 6667);
        assert!(!server.tls);
        assert!(!server.auto_connect);
        assert!(server.connect_commands.is_empty());
        assert_eq!(server.fingerprint, "");
        assert_eq!(server.sasl_password, "");
        assert_eq!(config.username, "");
        assert!(config.log);
    }

    #[test]
    fn invalid_file_is_an_error() {
        let file = TempFile::new("invalid");
        std::fs::write(&file.0, "nickname = [").unwrap();
        assert!(matches!(
            Config::load_from(&file.0),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn nick_falls_back_to_global() {
        let mut config = sample();
        let mut server = config.servers[0].clone();
        assert_eq!(config.effective_nick(&server), "tyler");
        server.nickname = "other".into();
        assert_eq!(config.effective_nick(&server), "other");
        server.nickname = "  ".into();
        assert_eq!(config.effective_nick(&server), "tyler");
        config.nickname = String::new();
        assert_eq!(config.effective_nick(&server), "");
    }

    #[test]
    fn username_and_realname_fall_back_to_nick() {
        let mut config = sample();
        let server = config.servers[0].clone();
        assert_eq!(config.effective_username(&server), "tyler");
        assert_eq!(config.effective_realname(&server), "Tyler");
        config.username = "tspivey".into();
        config.realname = String::new();
        assert_eq!(config.effective_username(&server), "tspivey");
        assert_eq!(config.effective_realname(&server), "tyler");
    }
}
