//! Self-update from GitHub releases.
//!
//! The latest release must carry `SpaceTree.exe` and `SpaceTree.exe.sha256` assets. The new exe is
//! downloaded next to the running one, its SHA-256 is verified, then the files are swapped: Windows lets
//! a running executable be renamed (not overwritten), so the old one becomes `SpaceTree.exe.old` and is
//! deleted on the next start. The new version runs after a restart.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

const API: &str = "https://api.github.com/repos/ahsoggg-creator/spacetree/releases/latest";
const EXE_ASSET: &str = "SpaceTree.exe";
const SUM_ASSET: &str = "SpaceTree.exe.sha256";
/// Do not ask GitHub more often than this (anonymous API limit is 60 requests per hour).
pub const CHECK_INTERVAL_SECS: u64 = 6 * 3600;
/// Download cap when GitHub does not announce the asset size.
const MAX_EXE_BYTES: u64 = 256 << 20;

#[derive(Clone, PartialEq)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    /// Newer version exists; not downloaded (auto-update off or the folder is not writable).
    Available {
        version: String,
        url: String,
    },
    Downloading {
        version: String,
        done: u64,
        total: u64,
    },
    /// Installed; takes effect after a restart.
    Ready {
        version: String,
    },
    Failed(String),
}

pub type Shared = Arc<Mutex<State>>;

struct Release {
    version: String,
    page: String,
    exe_url: String,
    exe_size: u64,
    sum_url: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .into()
}

fn ua() -> String {
    format!("SpaceTree/{}", env!("CARGO_PKG_VERSION"))
}

/// `"v1.2.3"` / `"1.2.3"` -> (1, 2, 3). Missing minor/patch count as 0; any component that is present must be
/// a number (`"1.x.3"` is rejected rather than read as 1.0.3).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let nums: Vec<u64> = core.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    if nums.len() > 3 {
        return None;
    }
    Some((
        nums[0],
        nums.get(1).copied().unwrap_or(0),
        nums.get(2).copied().unwrap_or(0),
    ))
}

/// Largest download accepted for an asset GitHub announced as `announced` bytes (0 = not announced).
fn download_limit(announced: u64) -> u64 {
    if announced > 0 { announced } else { MAX_EXE_BYTES }
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    matches!((parse_version(remote), parse_version(local)), (Some(r), Some(l)) if r > l)
}

fn latest() -> Result<Release, String> {
    let mut resp = agent()
        .get(API)
        .header("User-Agent", &ua())
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => "на GitHub пока нет релизов".to_owned(),
            e => format!("GitHub недоступен: {e}"),
        })?;
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("в ответе GitHub нет tag_name")?;
    let asset = |name: &str| {
        v["assets"].as_array().and_then(|a| {
            a.iter().find(|x| x["name"].as_str() == Some(name)).map(|x| {
                (
                    x["browser_download_url"].as_str().unwrap_or("").to_owned(),
                    x["size"].as_u64().unwrap_or(0),
                )
            })
        })
    };
    let (exe_url, exe_size) = asset(EXE_ASSET).ok_or("в релизе нет SpaceTree.exe")?;
    let (sum_url, _) = asset(SUM_ASSET).ok_or("в релизе нет контрольной суммы")?;
    Ok(Release {
        version: tag.trim_start_matches('v').to_owned(),
        page: v["html_url"].as_str().unwrap_or(crate::winint::REPO_URL).to_owned(),
        exe_url,
        exe_size,
        sum_url,
    })
}

fn target_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| e.to_string())
}

/// Deletes the previous executable left over by an update.
pub fn cleanup_old() {
    if let Ok(exe) = target_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
        let _ = std::fs::remove_file(exe.with_extension("exe.download"));
    }
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".spacetree-write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

fn download(rel: &Release, state: &Shared, repaint: &dyn Fn()) -> Result<(), String> {
    let exe = target_exe()?;
    let tmp = exe.with_extension("exe.download");

    let sum_text = agent()
        .get(&rel.sum_url)
        .header("User-Agent", &ua())
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let expected = sum_text
        .split_whitespace()
        .next()
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("некорректный файл контрольной суммы")?
        .to_ascii_lowercase();

    let mut resp = agent()
        .get(&rel.exe_url)
        .header("User-Agent", &ua())
        .call()
        .map_err(|e| e.to_string())?;
    let mut reader = resp.body_mut().as_reader();
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let limit = download_limit(rel.exe_size);
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(e.to_string());
            }
        };
        if n == 0 {
            break;
        }
        if done + n as u64 > limit {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            return Err("загрузка больше заявленного размера — обновление отменено".into());
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        *state.lock().unwrap() = State::Downloading {
            version: rel.version.clone(),
            done,
            total: rel.exe_size,
        };
        repaint();
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);

    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        let _ = std::fs::remove_file(&tmp);
        return Err("контрольная сумма не совпала — обновление отменено".into());
    }

    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("не удалось заменить exe: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::rename(&old, &exe); // roll back
        return Err(format!("не удалось заменить exe: {e}"));
    }
    Ok(())
}

/// Checks for a newer release in the background; with `install`, downloads and installs it.
pub fn spawn(state: Shared, install: bool, repaint: impl Fn() + Send + 'static) {
    {
        let mut st = state.lock().unwrap();
        // The new exe is already in place (the running one is `.exe.old`): another check would compare against
        // the old version, download again and fail to replace the running image.
        if matches!(*st, State::Ready { .. }) {
            return;
        }
        *st = State::Checking;
    }
    std::thread::spawn(move || {
        let result = (|| -> Result<State, String> {
            let rel = latest()?;
            if !is_newer(&rel.version, env!("CARGO_PKG_VERSION")) {
                return Ok(State::UpToDate);
            }
            let writable = target_exe()
                .ok()
                .and_then(|e| e.parent().map(dir_writable))
                .unwrap_or(false);
            if !install || !writable {
                return Ok(State::Available {
                    version: rel.version,
                    url: rel.page,
                });
            }
            download(&rel, &state, &repaint)?;
            Ok(State::Ready { version: rel.version })
        })();
        *state.lock().unwrap() = result.unwrap_or_else(State::Failed);
        repaint();
    });
}

/// Starts the (new) executable with the same arguments; the caller then closes its window.
pub fn restart() -> Result<(), String> {
    let exe = target_exe()?;
    std::process::Command::new(exe)
        .args(std::env::args().skip(1).filter(|a| !a.starts_with("--")))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10"), Some((0, 10, 0)));
        assert_eq!(parse_version("2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("x"), None);
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn malformed_versions_are_rejected() {
        assert_eq!(parse_version("1.x.3"), None);
        assert_eq!(parse_version("1..3"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1"), Some((1, 0, 0)));
        assert!(!is_newer("9.x", "0.3.1"));
    }

    #[test]
    fn download_is_capped() {
        assert_eq!(download_limit(5_000_000), 5_000_000);
        assert_eq!(download_limit(0), MAX_EXE_BYTES);
    }

    #[test]
    fn no_second_update_after_one_is_installed() {
        let state: Shared = Arc::new(Mutex::new(State::Ready {
            version: "9.9.9".into(),
        }));
        spawn(state.clone(), true, || {});
        assert!(matches!(*state.lock().unwrap(), State::Ready { .. }));
    }
}
