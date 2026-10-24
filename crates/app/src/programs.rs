//! Names of the programs installed on this computer, for telling leftover `AppData` folders from live ones
//! (`spacetree_core::orphans`). Read-only: the "Apps & features" registry keys and the Start menu.

use std::path::Path;

/// Display names, publishers, install folders and shortcut names; empty when nothing could be read.
pub fn installed_names() -> Vec<String> {
    let mut out = imp::registry_names();
    for var in ["ProgramData", "APPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            shortcut_names(
                &Path::new(&base).join(r"Microsoft\Windows\Start Menu\Programs"),
                2,
                &mut out,
            );
        }
    }
    out
}

/// File and folder names of the Start menu, `depth` levels deep (`.lnk` without the extension).
fn shortcut_names(dir: &Path, depth: u32, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            out.push(e.file_name().to_string_lossy().into_owned());
            if depth > 1 {
                shortcut_names(&path, depth - 1, out);
            }
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
            && let Some(stem) = path.file_stem()
        {
            out.push(stem.to_string_lossy().into_owned());
        }
    }
}

/// Last folder of an install path and the program's file stem: `"C:\Apps\Foo\foo.exe",0` -> `Foo`, `foo`.
#[cfg_attr(not(windows), allow(dead_code))]
fn path_names(raw: &str, out: &mut Vec<String>) {
    let p = raw.trim().trim_matches('"');
    let p = p
        .rsplit_once(',')
        .filter(|(_, idx)| idx.trim().parse::<i32>().is_ok())
        .map_or(p, |(a, _)| a);
    let path = Path::new(p.trim_matches('"'));
    let is_file = path.extension().is_some();
    if is_file && let Some(stem) = path.file_stem() {
        out.push(stem.to_string_lossy().into_owned());
    }
    let dir = if is_file { path.parent() } else { Some(path) };
    if let Some(name) = dir.and_then(Path::file_name) {
        out.push(name.to_string_lossy().into_owned());
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegEnumKeyExW,
        RegOpenKeyExW, RegQueryValueExW,
    };

    use super::path_names;

    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    const MAX_SUBKEYS: u32 = 20_000;

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegOpenKeyExW and is closed exactly once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open(root: HKEY, path: &str) -> Option<Key> {
        let p = wide(path);
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: `p` is NUL-terminated; the out-pointer references a live local.
        let rc = unsafe { RegOpenKeyExW(root, p.as_ptr(), 0, KEY_READ, &mut h) };
        (rc == ERROR_SUCCESS).then_some(Key(h))
    }

    fn subkey_name(key: &Key, index: u32) -> Option<String> {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` and `len` describe the same writable buffer; the optional out-parameters are null.
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                buf.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (rc == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
    }

    fn string_value(key: &Key, name: &str) -> Option<String> {
        let n = wide(name);
        let (mut ty, mut bytes) = (0u32, 0u32);
        // SAFETY: first call only asks for the type and size (null data pointer).
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                n.as_ptr(),
                std::ptr::null(),
                &mut ty,
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS || !(ty == REG_SZ || ty == REG_EXPAND_SZ) || bytes == 0 || bytes > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut cap = (buf.len() * 2) as u32;
        // SAFETY: `buf` is writable for `cap` bytes, which is passed as the buffer size.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                n.as_ptr(),
                std::ptr::null(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut cap,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let units = (cap as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..units]);
        let s = s.trim_end_matches('\0').trim();
        (!s.is_empty()).then(|| s.to_owned())
    }

    pub fn registry_names() -> Vec<String> {
        let mut out = Vec::new();
        for (root, path) in [
            (HKEY_LOCAL_MACHINE, UNINSTALL),
            (HKEY_LOCAL_MACHINE, UNINSTALL_32),
            (HKEY_CURRENT_USER, UNINSTALL),
        ] {
            let Some(base) = open(root, path) else { continue };
            for i in 0..MAX_SUBKEYS {
                let Some(name) = subkey_name(&base, i) else { break };
                let Some(app) = open(base.0, &name) else { continue };
                if !name.starts_with('{') {
                    out.push(name.clone()); // `Git_is1`, `Steam App 570`: the key is named after the program
                }
                for value in ["DisplayName", "Publisher"] {
                    out.extend(string_value(&app, value));
                }
                for value in ["InstallLocation", "DisplayIcon"] {
                    if let Some(raw) = string_value(&app, value) {
                        path_names(&raw, &mut out);
                    }
                }
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn registry_names() -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Windows machine has programs registered; an empty list would mean the reader is broken.
    #[cfg(windows)]
    #[test]
    fn the_registry_yields_installed_programs() {
        let names = installed_names();
        assert!(names.len() > 5, "{} names", names.len());
        assert!(names.iter().all(|n| !n.trim().is_empty()));
    }

    #[test]
    fn install_paths_give_folder_and_program_names() {
        let mut v = Vec::new();
        path_names(r#""C:\Program Files\Foo Bar\foo.exe",0"#, &mut v);
        path_names(r"D:\Games\Some Game\", &mut v);
        path_names("", &mut v);
        assert_eq!(v, ["foo", "Foo Bar", "Some Game"]);
    }
}
