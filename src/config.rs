//! Settings stored in `irctest.toml` in the current directory.

use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "irctest.toml";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Used for every server whose own nickname is blank.
    pub nickname: String,
    /// Blank = use the nickname.
    pub username: String,
    /// Blank = use the nickname.
    pub realname: String,
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// Sent as PASS when not blank. For ZNC: "user/network:pass".
    pub password: String,
    /// Blank = use the global nickname.
    pub nickname: String,
    pub auto_connect: bool,
    /// Raw IRC lines sent after registration (001).
    pub connect_commands: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 6667,
            tls: false,
            password: String::new(),
            nickname: String::new(),
            auto_connect: false,
            connect_commands: Vec::new(),
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
        if nick.is_empty() { self.nickname.trim() } else { nick }
    }

    pub fn effective_username<'a>(&'a self, server: &'a ServerConfig) -> &'a str {
        let user = self.username.trim();
        if user.is_empty() { self.effective_nick(server) } else { user }
    }

    pub fn effective_realname<'a>(&'a self, server: &'a ServerConfig) -> &'a str {
        let real = self.realname.trim();
        if real.is_empty() { self.effective_nick(server) } else { real }
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
            let path = std::env::temp_dir().join(format!("irctest-{}-{name}.toml", std::process::id()));
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
            servers: vec![ServerConfig {
                name: "lan".into(),
                host: "10.0.0.2".into(),
                port: 6667,
                tls: false,
                password: "user/net:pass".into(),
                nickname: String::new(),
                auto_connect: true,
                connect_commands: vec!["JOIN #test".into(), "PRIVMSG NickServ :IDENTIFY x".into()],
            }],
        }
    }

    #[test]
    fn missing_file_gives_default() {
        let file = TempFile::new("missing");
        assert_eq!(Config::load_from(&file.0).unwrap(), Config::default());
    }

    #[test]
    fn round_trip() {
        let file = TempFile::new("roundtrip");
        let config = sample();
        config.save_to(&file.0).unwrap();
        assert_eq!(Config::load_from(&file.0).unwrap(), config);
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
        assert_eq!(config.username, "");
    }

    #[test]
    fn invalid_file_is_an_error() {
        let file = TempFile::new("invalid");
        std::fs::write(&file.0, "nickname = [").unwrap();
        assert!(matches!(Config::load_from(&file.0), Err(ConfigError::Parse(_))));
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
