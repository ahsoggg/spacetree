//! Persistent user settings: `%APPDATA%\SpaceTree\settings.json` (or `~/.config/spacetree`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

const MAX_RECENT: usize = 8;

/// Demo mode (`--demo`): fake data for screenshots; nothing is written to disk.
static DEMO: AtomicBool = AtomicBool::new(false);

pub fn set_demo() {
    DEMO.store(true, Ordering::Relaxed);
}

pub fn is_demo() -> bool {
    DEMO.load(Ordering::Relaxed)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Download and install new releases from GitHub in the background.
    pub auto_update: bool,
    /// Unix seconds of the last update check (GitHub allows 60 anonymous API calls per hour).
    pub last_update_check: u64,
    /// Recently scanned paths, most recent first.
    pub recent: Vec<String>,
    /// Treemap coloring: by file type (false) or by modification age (true).
    pub color_by_age: bool,
    /// Delete to the recycle bin unless the user explicitly asks for permanent deletion.
    pub confirm_delete: bool,
    /// Save a snapshot of every scan to show what has grown since.
    pub save_snapshots: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            auto_update: true,
            last_update_check: 0,
            recent: Vec::new(),
            color_by_age: false,
            confirm_delete: true,
            save_snapshots: true,
        }
    }
}

pub fn dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("SpaceTree"))
    } else {
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config").join("spacetree"))
    }
}

fn file() -> Option<PathBuf> {
    dir().map(|d| d.join("settings.json"))
}

impl Settings {
    pub fn load() -> Settings {
        let Some(f) = file() else { return Settings::default() };
        let (settings, damaged) = match std::fs::read(&f) {
            Ok(b) => Self::parse(Some(&b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::parse(None),
            // Present but unreadable (locked by an antivirus, ...): same caution as a damaged file, but leave it.
            Err(_) => (Self::parse(Some(b"")).0, false),
        };
        if damaged && !is_demo() {
            // Keep the damaged file for inspection and write the cautious settings at once: otherwise a run that
            // ends without saving would leave no file, and the next start would fall back to auto-update on.
            if std::fs::rename(&f, f.with_extension("json.bad")).is_ok() {
                settings.save();
            }
        }
        settings
    }

    /// Settings from the file contents (`None` = no file); the flag is set when the file could not be parsed.
    /// A damaged file must not silently switch auto-update back on for a user who turned it off.
    fn parse(bytes: Option<&[u8]>) -> (Settings, bool) {
        let Some(b) = bytes else {
            return (Settings::default(), false);
        };
        match serde_json::from_slice(b) {
            Ok(s) => (s, false),
            Err(_) => (
                Settings {
                    auto_update: false,
                    ..Settings::default()
                },
                true,
            ),
        }
    }

    /// Best effort: settings are a convenience, failing to save them is not worth an error dialog.
    pub fn save(&self) {
        let Some(f) = file().filter(|_| !is_demo()) else { return };
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = f.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &f);
            }
        }
    }

    pub fn push_recent(&mut self, path: &str) {
        self.recent.retain(|p| !p.eq_ignore_ascii_case(path));
        self.recent.insert(0, path.to_owned());
        self.recent.truncate(MAX_RECENT);
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn damaged_settings_do_not_turn_auto_update_back_on() {
        let (s, corrupt) = Settings::parse(Some(br#"{"auto_update":false,"recent":null}"#));
        assert!(corrupt);
        assert!(!s.auto_update);
        let (s, corrupt) = Settings::parse(Some(b"\0\0garbage"));
        assert!(corrupt && !s.auto_update);
    }

    #[test]
    fn missing_or_valid_settings_load_normally() {
        assert!(Settings::parse(None).0.auto_update, "first start: default on");
        let (s, corrupt) = Settings::parse(Some(br#"{"auto_update":false}"#));
        assert!(!corrupt && !s.auto_update);
        let (s, _) = Settings::parse(Some(br#"{"recent":["C:\\"]}"#));
        assert!(s.auto_update && s.recent == ["C:\\"], "missing fields take defaults");
    }
}
