//! Windows integration: per-user installation (no administrator rights needed), Start menu and desktop
//! shortcuts, the "Scan with SpaceTree" Explorer context menu, the "Apps & features" entry and
//! deletion to the recycle bin.
//!
//! Everything is written under HKCU / the user's profile, so installing never needs UAC.

use std::path::{Path, PathBuf};

pub const APP: &str = "SpaceTree";
pub const REPO_URL: &str = "https://github.com/ahsoggg-creator/spacetree";

#[cfg(windows)]
pub use imp::*;

#[derive(Clone, Copy)]
pub struct InstallOptions {
    pub desktop_shortcut: bool,
    pub context_menu: bool,
}

/// `%LOCALAPPDATA%\Programs\SpaceTree`.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs").join(APP))
}

pub fn installed_exe() -> Option<PathBuf> {
    install_dir().map(|d| d.join(format!("{APP}.exe")))
}

fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    norm(a) == norm(b)
}

/// The running executable is the installed copy.
pub fn running_installed() -> bool {
    match (std::env::current_exe(), installed_exe()) {
        (Ok(cur), Some(inst)) => same_file(&cur, &inst),
        _ => false,
    }
}

/// The executable was started as the installer (`SpaceTree-Setup.exe` or `--install`).
pub fn is_setup_launch() -> bool {
    std::env::args().any(|a| a == "--install")
        || std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
            .is_some_and(|s| s.contains("setup"))
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
        SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, SHEmptyRecycleBinW, SHFILEOPSTRUCTW, SHFileOperationW,
    };

    use super::{APP, InstallOptions, REPO_URL, install_dir, installed_exe, same_file};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\SpaceTree";
    const APP_PATHS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\SpaceTree.exe";
    /// Folder, folder background and drive context menus.
    const MENU_KEYS: [(&str, &str); 3] = [
        (r"Software\Classes\Directory\shell\SpaceTree", "%1"),
        (r"Software\Classes\Directory\Background\shell\SpaceTree", "%V"),
        (r"Software\Classes\Drive\shell\SpaceTree", "%1"),
    ];

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegCreateKeyExW / RegOpenKeyExW and is closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn create_key(path: &str) -> Result<Key, String> {
        let p = wide(path);
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: `p` is NUL-terminated; out-pointer references a live local; no class / security attrs.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                p.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut h,
                std::ptr::null_mut(),
            )
        };
        if rc == ERROR_SUCCESS {
            Ok(Key(h))
        } else {
            Err(format!("реестр: не удалось создать {path} (код {rc})"))
        }
    }

    fn set_str(key: &Key, name: Option<&str>, value: &str) -> Result<(), String> {
        let n = name.map(wide);
        let v = wide(value);
        // SAFETY: `v` is a NUL-terminated UTF-16 buffer whose byte length is passed; name is NUL-terminated or null.
        let rc = unsafe {
            RegSetValueExW(
                key.0,
                n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                v.as_ptr() as *const u8,
                (v.len() * 2) as u32,
            )
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("реестр: запись значения не удалась (код {rc})"))
        }
    }

    fn set_dword(key: &Key, name: &str, value: u32) -> Result<(), String> {
        let n = wide(name);
        // SAFETY: the data pointer references a live u32 of the stated size.
        let rc = unsafe { RegSetValueExW(key.0, n.as_ptr(), 0, REG_DWORD, &value as *const u32 as *const u8, 4) };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("реестр: запись значения не удалась (код {rc})"))
        }
    }

    fn key_exists(path: &str) -> bool {
        let p = wide(path);
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: `p` is NUL-terminated; the out-pointer references a live local.
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, KEY_READ, &mut h) };
        if rc == ERROR_SUCCESS {
            drop(Key(h));
            true
        } else {
            false
        }
    }

    fn delete_tree(path: &str) {
        let p = wide(path);
        // SAFETY: `p` is NUL-terminated; deleting a missing key just returns an error code.
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, p.as_ptr()) };
    }

    fn notify_shell() {
        // SAFETY: SHCNE_ASSOCCHANGED takes no items.
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
    }

    /// Runs a PowerShell snippet without a console window. The script travels base64-encoded, so paths
    /// with quotes or non-ASCII characters need no escaping beyond PowerShell's own single quotes.
    fn powershell(script: &str) -> Result<(), String> {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let encoded = base64(&utf16);
        let out = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                &encoded,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("не удалось запустить PowerShell: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("PowerShell: {}", String::from_utf8_lossy(&out.stderr).trim()))
        }
    }

    fn base64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    fn ps_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    /// Start menu (searchable as "SpaceTree") and, optionally, desktop shortcuts.
    fn create_shortcuts(exe: &Path, desktop: bool) -> Result<(), String> {
        let exe = ps_quote(&exe.to_string_lossy());
        let mut script = format!(
            "$ws = New-Object -ComObject WScript.Shell\n\
             function Make($dir) {{ $l = $ws.CreateShortcut((Join-Path $dir 'SpaceTree.lnk')); \
             $l.TargetPath = {exe}; $l.IconLocation = {exe} + ',0'; \
             $l.Description = 'SpaceTree — анализ занятого места на диске'; $l.Save() }}\n\
             Make ([Environment]::GetFolderPath('Programs'))\n"
        );
        if desktop {
            script.push_str("Make ([Environment]::GetFolderPath('Desktop'))\n");
        }
        powershell(&script)
    }

    fn remove_shortcuts() {
        let _ = powershell(
            "foreach ($f in 'Programs','Desktop') { \
             $p = Join-Path ([Environment]::GetFolderPath($f)) 'SpaceTree.lnk'; \
             if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Force } }",
        );
    }

    pub fn context_menu_enabled() -> bool {
        key_exists(MENU_KEYS[0].0)
    }

    /// Adds or removes "Сканировать в SpaceTree" for folders, folder backgrounds and drives.
    /// On Windows 11 it lives under "Показать дополнительные параметры" (Shift+F10), like other classic entries.
    pub fn set_context_menu(enable: bool, exe: &Path) -> Result<(), String> {
        for (key, arg) in MENU_KEYS {
            if enable {
                let k = create_key(key)?;
                set_str(&k, None, "Сканировать в SpaceTree")?;
                set_str(&k, Some("Icon"), &format!("\"{}\",0", exe.display()))?;
                let cmd = create_key(&format!(r"{key}\command"))?;
                set_str(&cmd, None, &format!("\"{}\" \"{arg}\"", exe.display()))?;
            } else {
                delete_tree(key);
            }
        }
        notify_shell();
        Ok(())
    }

    /// Copies the running executable into `%LOCALAPPDATA%\Programs\SpaceTree` and registers it.
    /// Returns the installed executable.
    pub fn install(opts: InstallOptions) -> Result<PathBuf, String> {
        let dir = install_dir().ok_or("не найдена папка LOCALAPPDATA")?;
        let exe = installed_exe().ok_or("не найдена папка LOCALAPPDATA")?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let cur = std::env::current_exe().map_err(|e| e.to_string())?;
        if !same_file(&cur, &exe) {
            // A running older copy cannot be overwritten, but it can be renamed out of the way.
            if exe.exists() && std::fs::remove_file(&exe).is_err() {
                let old = exe.with_extension("exe.old");
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
            }
            std::fs::copy(&cur, &exe).map_err(|e| format!("копирование в {}: {e}", exe.display()))?;
        }

        create_shortcuts(&exe, opts.desktop_shortcut)?;
        set_context_menu(opts.context_menu, &exe)?;

        let ap = create_key(APP_PATHS_KEY)?; // Win+R "spacetree"
        set_str(&ap, None, &exe.to_string_lossy())?;

        let u = create_key(UNINSTALL_KEY)?;
        let exe_s = exe.to_string_lossy();
        set_str(&u, Some("DisplayName"), APP)?;
        set_str(&u, Some("DisplayVersion"), env!("CARGO_PKG_VERSION"))?;
        set_str(&u, Some("Publisher"), APP)?;
        set_str(&u, Some("DisplayIcon"), &format!("{exe_s},0"))?;
        set_str(&u, Some("InstallLocation"), &dir.to_string_lossy())?;
        set_str(&u, Some("UninstallString"), &format!("\"{exe_s}\" --uninstall"))?;
        set_str(&u, Some("URLInfoAbout"), REPO_URL)?;
        set_dword(&u, "NoModify", 1)?;
        set_dword(&u, "NoRepair", 1)?;
        let kb = std::fs::metadata(&exe).map_or(0, |m| m.len() / 1024) as u32;
        set_dword(&u, "EstimatedSize", kb)?;
        Ok(exe)
    }

    /// Updates the version shown in "Apps & features" after a self-update.
    pub fn refresh_installed_version() {
        if key_exists(UNINSTALL_KEY)
            && let Ok(k) = create_key(UNINSTALL_KEY)
        {
            let _ = set_str(&k, Some("DisplayVersion"), env!("CARGO_PKG_VERSION"));
        }
    }

    /// Removes shortcuts, registry entries and settings, then deletes the install folder once this
    /// process has exited.
    pub fn uninstall() -> Result<(), String> {
        remove_shortcuts();
        for (key, _) in MENU_KEYS {
            delete_tree(key);
        }
        delete_tree(APP_PATHS_KEY);
        delete_tree(UNINSTALL_KEY);
        notify_shell();
        if let Some(d) = crate::settings::dir() {
            let _ = std::fs::remove_dir_all(d);
        }
        if let Some(dir) = install_dir()
            && dir.exists()
        {
            // The running exe cannot delete itself: a detached PowerShell waits until we exit, then removes it.
            let script = format!(
                "Wait-Process -Id {} -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 300;                  Remove-Item -LiteralPath {} -Recurse -Force",
                std::process::id(),
                ps_quote(&dir.to_string_lossy())
            );
            let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &base64(&utf16)])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Moves one file or folder to the recycle bin.
    pub fn recycle(path: &str) -> Result<(), String> {
        // pFrom is a list of NUL-terminated strings ending with an extra NUL.
        let mut from: Vec<u16> = path.encode_utf16().collect();
        from.extend([0, 0]);
        let mut op = SHFILEOPSTRUCTW {
            hwnd: std::ptr::null_mut(),
            wFunc: FO_DELETE,
            pFrom: from.as_ptr(),
            pTo: std::ptr::null(),
            // FOF_WANTNUKEWARNING: an item that cannot go to the bin (too big, no bin on that drive) would
            // otherwise be destroyed silently although the user chose "to the recycle bin"; Windows asks first.
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING) as u16,
            fAnyOperationsAborted: 0,
            hNameMappings: std::ptr::null_mut(),
            lpszProgressTitle: std::ptr::null(),
        };
        // SAFETY: `op` is fully initialised and `from` (double-NUL terminated) outlives the call.
        let rc = unsafe { SHFileOperationW(&mut op) };
        if rc != 0 {
            Err(format!("не удалось переместить в корзину (код 0x{rc:X})"))
        } else if op.fAnyOperationsAborted != 0 {
            Err("операция прервана".into())
        } else {
            Ok(())
        }
    }

    /// Empties the recycle bin of the drive `drive` only (the one whose bin size the user was shown), without
    /// Windows' own confirmation (we ask ourselves).
    pub fn empty_recycle_bin(drive: char) -> Result<(), String> {
        const SHERB_NOCONFIRMATION: u32 = 1;
        const SHERB_NOPROGRESSUI: u32 = 2;
        const SHERB_NOSOUND: u32 = 4;
        let root: Vec<u16> = format!("{drive}:\\").encode_utf16().chain([0]).collect();
        // SAFETY: `root` is NUL-terminated and outlives the call; no window handle.
        let hr = unsafe {
            SHEmptyRecycleBinW(
                std::ptr::null_mut(),
                root.as_ptr(),
                SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
            )
        };
        // S_OK, or E_UNEXPECTED-ish codes when the bin is already empty
        if hr >= 0 || hr == 0x8000FFFFu32 as i32 {
            Ok(())
        } else {
            Err(format!("не удалось очистить корзину (0x{:X})", hr as u32))
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn base64_matches_reference() {
            assert_eq!(super::base64(b""), "");
            assert_eq!(super::base64(b"f"), "Zg==");
            assert_eq!(super::base64(b"fo"), "Zm8=");
            assert_eq!(super::base64(b"foo"), "Zm9v");
            assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
        }
    }
}

#[cfg(not(windows))]
mod imp_stub {
    use std::path::{Path, PathBuf};

    use super::InstallOptions;

    const NA: &str = "доступно только в Windows";

    pub fn context_menu_enabled() -> bool {
        false
    }
    pub fn set_context_menu(_: bool, _: &Path) -> Result<(), String> {
        Err(NA.into())
    }
    pub fn install(_: InstallOptions) -> Result<PathBuf, String> {
        Err(NA.into())
    }
    pub fn refresh_installed_version() {}
    pub fn uninstall() -> Result<(), String> {
        Err(NA.into())
    }
    pub fn recycle(_: &str) -> Result<(), String> {
        Err(NA.into())
    }
    pub fn empty_recycle_bin(_drive: char) -> Result<(), String> {
        Err(NA.into())
    }
}
#[cfg(not(windows))]
pub use imp_stub::*;
