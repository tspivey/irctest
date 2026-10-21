//! Conversation logs: `logs/<server>/<target>/YYYY-MM-DD.log`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate};

use crate::format::strip_formatting;

pub const LOG_DIR: &str = "logs";

/// The folder name for the server tab's log.
pub const SERVER_TARGET: &str = "server";

/// Stable log namespace for an account and an opaque bouncer network ID.
/// Length prefixes prevent component-boundary collisions; the digest keeps
/// account details and filesystem-unsafe network IDs out of directory names.
pub fn child_namespace(account_identity: &str, network_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for component in [account_identity, network_id] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component.as_bytes());
    }
    format!("soju-{:x}", digest.finalize())
}

/// Makes a name safe as a Windows file or folder name. Characters Windows doesn't allow and
/// trailing dots and spaces become `_`; device names like `NUL` or `con.foo` get a `_` added.
pub fn sanitize(name: &str) -> String {
    let mut out: Vec<char> = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    for c in out.iter_mut().rev() {
        if *c != '.' && *c != ' ' {
            break;
        }
        *c = '_';
    }
    let mut out: String = out.into_iter().collect();
    if out.is_empty() || is_reserved(&out) {
        out.push('_');
    }
    out
}

/// Windows device names, which are reserved with any extension and in any case.
fn is_reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let (prefix, digit) = stem.split_at(stem.len().min(3));
            (prefix == "COM" || prefix == "LPT")
                && digit.len() == 1
                && matches!(digit.as_bytes()[0], b'1'..=b'9')
        }
    }
}

/// The folder holding one target's logs, under `dir`.
pub fn log_folder(dir: &Path, server: &str, target: &str) -> PathBuf {
    dir.join(sanitize(server)).join(sanitize(target))
}

/// The log file for one target and day, under `dir`.
pub fn log_path(dir: &Path, server: &str, target: &str, date: NaiveDate) -> PathBuf {
    log_folder(dir, server, target).join(format!("{}.log", date.format("%Y-%m-%d")))
}

/// Open log files, kept open between lines. Each line is written straight to its file
/// (no buffering), so nothing is lost if the program crashes.
pub struct Logger {
    dir: PathBuf,
    /// Keyed by lowercased folder, since Windows names ignore case.
    files: HashMap<String, (NaiveDate, File)>,
}

impl Default for Logger {
    fn default() -> Self {
        Self::new(LOG_DIR)
    }
}

impl Logger {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            files: HashMap::new(),
        }
    }

    /// Appends `[HH:MM:SS] text` to the target's log for `time`'s date, without formatting codes.
    pub fn write(
        &mut self,
        server: &str,
        target: &str,
        time: DateTime<Local>,
        text: &str,
    ) -> io::Result<()> {
        let key = self.key(server, target);
        let date = time.date_naive();
        if !matches!(self.files.get(&key), Some((open_date, _)) if *open_date == date) {
            // Not open yet, or a new day: drop any old handle first, so a failed open
            // doesn't leave yesterday's file in use.
            self.files.remove(&key);
            let path = log_path(&self.dir, server, target, date);
            if let Some(folder) = path.parent() {
                std::fs::create_dir_all(folder)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.files.insert(key.clone(), (date, file));
        }
        let (_, file) = self.files.get_mut(&key).expect("opened above");
        let line = format!("[{}] {}\n", time.format("%H:%M:%S"), strip_formatting(text));
        file.write_all(line.as_bytes())
    }

    /// Closes a target's log file, e.g. when its tab is closed or renamed.
    pub fn close(&mut self, server: &str, target: &str) {
        let key = self.key(server, target);
        self.files.remove(&key);
    }

    pub fn close_all(&mut self) {
        self.files.clear();
    }

    fn key(&self, server: &str, target: &str) -> String {
        log_folder(&self.dir, server, target)
            .to_string_lossy()
            .to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn child_namespaces_are_stable_and_do_not_collapse_components() {
        let account = "account/one:private-ish";
        let namespace = child_namespace(account, "network/id");
        assert_eq!(namespace, child_namespace(account, "network/id"));
        assert_eq!(sanitize(&namespace), namespace);
        assert_eq!(namespace.len(), "soju-".len() + 64);
        assert!(!namespace.contains(account));
        assert_ne!(namespace, child_namespace("other", "network/id"));
        assert_ne!(namespace, child_namespace(account, "network_id"));
        assert_ne!(child_namespace("ab", "c"), child_namespace("a", "bc"));
        assert_ne!(
            child_namespace("Account", "net"),
            child_namespace("account", "net")
        );
    }
    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize("#test"), "#test");
        assert_eq!(sanitize("bob|away"), "bob_away");
        assert_eq!(sanitize(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("tab\there"), "tab_here");
        assert_eq!(sanitize("dots..."), "dots___");
        assert_eq!(sanitize("space "), "space_");
        assert_eq!(sanitize("."), "_");
        assert_eq!(sanitize(".."), "__");
        assert_eq!(sanitize(".hidden"), ".hidden");
        assert_eq!(sanitize(""), "_");
        assert_eq!(sanitize("[bob]^"), "[bob]^");
        assert_eq!(sanitize("héllo"), "héllo");
    }

    #[test]
    fn reserved_names() {
        assert_eq!(sanitize("nul"), "nul_");
        assert_eq!(sanitize("CON"), "CON_");
        assert_eq!(sanitize("Aux"), "Aux_");
        assert_eq!(sanitize("prn.foo"), "prn.foo_");
        assert_eq!(sanitize("com1"), "com1_");
        assert_eq!(sanitize("LPT9"), "LPT9_");
        assert_eq!(sanitize("com0"), "com0");
        assert_eq!(sanitize("com10"), "com10");
        assert_eq!(sanitize("console"), "console");
        assert_eq!(sanitize("nullable"), "nullable");
    }

    #[test]
    fn paths() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        assert_eq!(
            log_path(Path::new("logs"), "lan", "#test", date),
            Path::new("logs")
                .join("lan")
                .join("#test")
                .join("2026-10-13.log")
        );
        assert_eq!(
            log_path(Path::new("logs"), "my:net", "bob|away", date),
            Path::new("logs")
                .join("my_net")
                .join("bob_away")
                .join("2026-10-13.log")
        );
    }

    /// A folder in the system temp dir that is removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("irctest-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn writes_lines_per_day() {
        let dir = TempDir::new("logs");
        let mut logger = Logger::new(&dir.0);
        let day1 = Local.with_ymd_and_hms(2026, 10, 13, 23, 59, 58).unwrap();
        let day2 = Local.with_ymd_and_hms(2026, 10, 14, 0, 0, 1).unwrap();
        logger
            .write("lan", "#test", day1, "<bob> \x0304red\x03 \x02bold")
            .unwrap();
        logger
            .write("lan", "#Test", day1, "<tyler> same folder")
            .unwrap();
        logger
            .write("lan", "#test", day2, "<bob> next day")
            .unwrap();
        logger.close("lan", "#test");
        logger
            .write("lan", "#test", day2, "<bob> reopened")
            .unwrap();

        let read = |date: (i32, u32, u32)| {
            let date = NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap();
            std::fs::read_to_string(log_path(&dir.0, "lan", "#test", date)).unwrap()
        };
        assert_eq!(
            read((2026, 10, 13)),
            "[23:59:58] <bob> red bold\n[23:59:58] <tyler> same folder\n"
        );
        assert_eq!(
            read((2026, 10, 14)),
            "[00:00:01] <bob> next day\n[00:00:01] <bob> reopened\n"
        );
    }
}
