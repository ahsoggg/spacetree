//! Heuristic search for space that can most likely be reclaimed: temp files, caches, crash dumps,
//! build artifacts, old installers and the like.
//!
//! Rules look only at names, paths, sizes and dates in the scanned tree — nothing is opened or deleted.
//! A matched directory is reported as a whole and not descended into, so nothing is counted twice.

use crate::stats::ext_raw;
use crate::tree::{Kind, NodeId, Tree};

/// How confident the rule is that deleting is harmless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Safety {
    /// Recreated automatically or meant to be thrown away.
    Safe,
    /// Usually harmless, but costs a re-download / rebuild or may contain something wanted.
    Likely,
    /// Look before deleting.
    Review,
}

impl Safety {
    pub fn label(self) -> &'static str {
        match self {
            Safety::Safe => "безопасно",
            Safety::Likely => "скорее безопасно",
            Safety::Review => "проверьте",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    RecycleBin,
    Temp,
    BrowserCache,
    AppCache,
    Thumbnails,
    CrashDumps,
    WindowsUpdate,
    OldWindows,
    TempFiles,
    Logs,
    PackageCache,
    DevArtifacts,
    DriverInstallers,
    AiModels,
    VirtualDisks,
    Hibernation,
    Backups,
    OldDownloads,
    Duplicates,
    BigOld,
    OrphanedData,
    OrphanedGames,
}

impl Category {
    pub const ALL: [Category; 22] = [
        Category::RecycleBin,
        Category::Temp,
        Category::BrowserCache,
        Category::AppCache,
        Category::Thumbnails,
        Category::CrashDumps,
        Category::WindowsUpdate,
        Category::OldWindows,
        Category::TempFiles,
        Category::Logs,
        Category::PackageCache,
        Category::DevArtifacts,
        Category::DriverInstallers,
        Category::AiModels,
        Category::VirtualDisks,
        Category::Hibernation,
        Category::Backups,
        Category::OldDownloads,
        Category::Duplicates,
        Category::BigOld,
        Category::OrphanedData,
        Category::OrphanedGames,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::RecycleBin => "Корзина",
            Category::Temp => "Временные папки",
            Category::BrowserCache => "Кэш браузеров",
            Category::AppCache => "Кэш приложений",
            Category::Thumbnails => "Кэш эскизов Проводника",
            Category::CrashDumps => "Дампы памяти и отчёты об ошибках",
            Category::WindowsUpdate => "Загрузки Центра обновления Windows",
            Category::OldWindows => "Предыдущая установка Windows",
            Category::TempFiles => "Временные файлы",
            Category::Logs => "Журналы (логи)",
            Category::PackageCache => "Кэш менеджеров пакетов",
            Category::DevArtifacts => "Зависимости и артефакты сборки",
            Category::DriverInstallers => "Распакованные установщики драйверов",
            Category::AiModels => "Локальные ИИ-модели",
            Category::VirtualDisks => "Виртуальные диски Docker и WSL",
            Category::Hibernation => "Файл гибернации",
            Category::Backups => "Резервные копии файлов",
            Category::OldDownloads => "Старые установщики и архивы в «Загрузках»",
            Category::Duplicates => "Дубликаты (одинаковое содержимое)",
            Category::BigOld => "Крупные файлы, не менявшиеся больше года",
            Category::OrphanedData => "Осиротевшие данные программ (Orphaned)",
            Category::OrphanedGames => "Остатки игр Steam и Epic Games (Orphaned)",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Category::RecycleBin => "Удалённые файлы. Очистите корзину, если ничего не нужно восстановить.",
            Category::Temp => {
                "Папки TEMP пользователя и Windows. Файлы, занятые запущенными программами, просто не удалятся."
            }
            Category::BrowserCache => {
                "Кэш страниц и шейдеров. Браузер создаст его заново; закройте браузер перед очисткой."
            }
            Category::AppCache => "Кэш программ (Electron, видеодрайверы и т. п.). Создаётся заново при запуске.",
            Category::Thumbnails => "Миниатюры картинок и значков. Проводник пересоздаст их при просмотре папок.",
            Category::CrashDumps => "Нужны только для диагностики сбоев.",
            Category::WindowsUpdate => {
                "Уже установленные обновления. Безопаснее чистить через «Очистку диска» или «Параметры › Память»."
            }
            Category::OldWindows => {
                "Нужна только для отката на прошлую версию. Удаляйте через «Очистку диска» › «Предыдущие установки Windows»."
            }
            Category::TempFiles => "Файлы *.tmp и ~$-файлы Office (если документ сейчас не открыт).",
            Category::Logs => "Журналы программ. Обычно не нужны, но иногда помогают разобраться в проблеме.",
            Category::PackageCache => {
                "npm, pip, cargo, NuGet, Gradle, conda и др. Пакеты скачаются заново при следующей сборке."
            }
            Category::DevArtifacts => {
                "node_modules, target, bin/obj, __pycache__ и т. п. Восстанавливаются сборкой проекта (npm install, cargo build…)."
            }
            Category::DriverInstallers => {
                "Папки C:/NVIDIA и C:/AMD:распакованные установщики драйверов видеокарты. Установленный драйвер от них не зависит."
            }
            Category::AiModels => {
                "Модели Ollama и LM Studio. Скачаются заново, но это десятки гигабайт трафика — удаляйте ненужные модели."
            }
            Category::VirtualDisks => {
                "Не удаляйте: внутри данные Docker/WSL. Диск только растёт — сожмите его: wsl --shutdown, затем от                  администратора diskpart › select vdisk file=\"путь\" › attach vdisk readonly › compact vdisk › detach vdisk                  (или Optimize-VHD -Path путь -Mode Full)."
            }
            Category::Hibernation => {
                "Удаляется только отключением гибернации: powercfg /h off (от администратора). Пропадёт быстрый запуск."
            }
            Category::Backups => "Файлы *.bak, *.old, *~. Убедитесь, что оригиналы на месте.",
            Category::OldDownloads => "Установщики и архивы старше 30 дней. Программы обычно уже установлены.",
            Category::Duplicates => {
                "Файлы от 1 МБ с полностью совпадающим содержимым (проверено по хешу). Самая новая копия сюда не входит — наведите курсор на строку, чтобы увидеть, какая оставлена."
            }
            Category::BigOld => "Файлы от 1 ГБ без изменений больше года (кроме системных папок). Решайте сами.",
            Category::OrphanedData => {
                "Папки в AppData от 50 МБ, имя которых не совпадает ни с одной установленной программой (реестр, Program Files). Возможно, программа уже удалена — но это догадка по названию: портативные программы и папки, названные по издателю, тоже могут попасть сюда."
            }
            Category::OrphanedGames => {
                "Папки игр, о которых Steam или Epic Games Launcher больше не знает (нет манифеста установки), и остатки Proton/шейдеров удалённых игр Steam. Убедитесь, что игра не установлена вручную."
            }
        }
    }

    pub fn safety(self) -> Safety {
        match self {
            Category::RecycleBin
            | Category::Temp
            | Category::BrowserCache
            | Category::Thumbnails
            | Category::CrashDumps => Safety::Safe,
            Category::AppCache
            | Category::WindowsUpdate
            | Category::OldWindows
            | Category::TempFiles
            | Category::Logs
            | Category::PackageCache
            | Category::DevArtifacts
            | Category::DriverInstallers => Safety::Likely,
            Category::AiModels
            | Category::VirtualDisks
            | Category::Hibernation
            | Category::Backups
            | Category::OldDownloads
            | Category::Duplicates
            | Category::BigOld
            | Category::OrphanedData
            | Category::OrphanedGames => Safety::Review,
        }
    }
}

pub struct Group {
    pub category: Category,
    /// Matched files and folders, largest first.
    pub items: Vec<NodeId>,
    pub size: u64,
    pub alloc: u64,
    pub files: u64,
}

const DAY: u32 = 86_400;
const OLD_DOWNLOAD_AGE: u32 = 30 * DAY;
const BIG_OLD_AGE: u32 = 365 * DAY;
const BIG_OLD_SIZE: u64 = 1 << 30;
/// Smallest file considered for the duplicates check.
const DUP_MIN_SIZE: u64 = 1 << 20;
/// Only personal content is checked for duplicates: copies of program files are usually intentional.
const DUP_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "mp3", "flac", "wav", "m4a", "ogg", "aac", "jpg", "jpeg", "png",
    "gif", "webp", "heic", "bmp", "tif", "tiff", "psd", "raw", "cr2", "nef", "arw", "dng", "pdf", "djvu", "epub",
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "zip", "rar", "7z", "iso", "msi",
];
/// Groups smaller than this are noise (e.g. the `desktop.ini` of an empty recycle bin).
const MIN_GROUP: u64 = 64 << 10;

const BROWSER_PROFILES: &[&str] = &[
    "/google/chrome/user data/",
    "/google/chrome beta/user data/",
    "/microsoft/edge/user data/",
    "/yandex/yandexbrowser/user data/",
    "/bravesoftware/brave-browser/user data/",
    "/vivaldi/user data/",
    "/chromium/user data/",
    "/opera software/",
    "/mozilla/firefox/profiles/",
];
const BROWSER_CACHE_NAMES: &[&str] = &[
    "cache",
    "cache2",
    "code cache",
    "gpucache",
    "cache_data",
    "shadercache",
    "grshadercache",
    "graphitedawncache",
    "dawncache",
    "dawngraphitecache",
    "dawnwebgpucache",
    "startupcache",
];
/// Unambiguous cache folder names, accepted anywhere under AppData.
const APP_CACHE_NAMES: &[&str] = &[
    "code cache",
    "gpucache",
    "cacheddata",
    "cachedextensionvsixs",
    "shadercache",
    "dxcache",
    "glcache",
    "d3dscache",
    "nv_cache",
    "cache_data",
    "dawncache",
    "dawngraphitecache",
    "dawnwebgpucache",
];
/// Path suffixes of whole directories, with their category.
const DIR_SUFFIXES: &[(&str, Category)] = &[
    ("/appdata/local/temp", Category::Temp),
    ("/microsoft/windows/inetcache", Category::BrowserCache),
    ("/appdata/local/crashdumps", Category::CrashDumps),
    ("/microsoft/windows/wer/reportarchive", Category::CrashDumps),
    ("/microsoft/windows/wer/reportqueue", Category::CrashDumps),
    ("/windows/softwaredistribution/download", Category::WindowsUpdate),
    ("/deliveryoptimization/cache", Category::WindowsUpdate),
    ("/.npm/_cacache", Category::PackageCache),
    ("/appdata/local/npm-cache", Category::PackageCache),
    ("/appdata/roaming/npm-cache", Category::PackageCache),
    ("/appdata/local/pip/cache", Category::PackageCache),
    ("/appdata/local/pypoetry/cache", Category::PackageCache),
    ("/appdata/local/uv/cache", Category::PackageCache),
    ("/.cargo/registry/cache", Category::PackageCache),
    ("/.nuget/packages", Category::PackageCache),
    ("/appdata/local/pnpm/store", Category::PackageCache),
    ("/appdata/local/composer", Category::PackageCache),
    ("/appdata/local/vcpkg/archives", Category::PackageCache),
    ("/.ollama/models", Category::AiModels),
    ("/.lmstudio/models", Category::AiModels),
    ("/steamapps/shadercache", Category::AppCache),
    ("/programdata/nvidia corporation/nv_cache", Category::AppCache),
    ("/.cargo/registry/src", Category::PackageCache),
    ("/appdata/local/nuget/v3-cache", Category::PackageCache),
    ("/appdata/local/yarn/cache", Category::PackageCache),
    ("/appdata/local/yarn/berry/cache", Category::PackageCache),
    ("/appdata/local/go-build", Category::PackageCache),
    ("/.gradle/caches", Category::PackageCache),
    ("/.gradle/wrapper/dists", Category::PackageCache),
    ("/appdata/local/composer/cache", Category::PackageCache),
    ("/appdata/local/electron/cache", Category::PackageCache),
    ("/appdata/local/electron-builder/cache", Category::PackageCache),
    ("/anaconda3/pkgs", Category::PackageCache),
    ("/miniconda3/pkgs", Category::PackageCache),
];
/// Directories under the volume root (`X:/name`).
const VOLUME_DIRS: &[(&str, Category)] = &[
    ("$recycle.bin", Category::RecycleBin),
    ("windows.old", Category::OldWindows),
    ("$windows.~bt", Category::OldWindows),
    ("$windows.~ws", Category::OldWindows),
    ("nvidia", Category::DriverInstallers),
    ("amd", Category::DriverInstallers),
];
/// System folders where the walk does not look for loose files to delete.
const SKIP_UNDER_VOLUME: &[&str] = &[
    "windows/winsxs",
    "windows/installer",
    "windows/servicing",
    "windows/assembly",
    "windows/system32/driverstore",
];
const SYSTEM_UNDER_VOLUME: &[&str] = &["windows", "program files", "program files (x86)", "programdata"];
/// Build output folders recognised by a marker file next to them: (folder, marker suffix).
const BUILD_DIRS: &[(&str, &str)] = &[
    ("node_modules", "package.json"),
    ("target", "cargo.toml"),
    (".next", "package.json"),
    (".nuxt", "package.json"),
    (".svelte-kit", "package.json"),
    (".angular", "angular.json"),
    (".parcel-cache", "package.json"),
    (".turbo", "package.json"),
    ("bin", ".csproj"),
    ("obj", ".csproj"),
    ("bin", ".fsproj"),
    ("obj", ".fsproj"),
    ("bin", ".vbproj"),
    ("obj", ".vbproj"),
    ("build", "build.gradle"),
    ("build", "build.gradle.kts"),
    (".gradle", "build.gradle"),
    (".gradle", "build.gradle.kts"),
    (".gradle", "settings.gradle"),
];
/// Build/tool caches that are disposable wherever they are.
const DEV_CACHE_NAMES: &[&str] = &[
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    "cmake-build-debug",
    "cmake-build-release",
];
const ARCHIVE_EXTS: &[&str] = &[
    "exe",
    "msi",
    "msix",
    "msixbundle",
    "appx",
    "appxbundle",
    "iso",
    "img",
    "zip",
    "rar",
    "7z",
    "tar",
    "gz",
    "tgz",
    "xz",
    "bz2",
    "cab",
];

fn ext_is(name: &str, list: &[&str]) -> bool {
    let e = ext_raw(name);
    !e.is_empty() && list.iter().any(|x| e.eq_ignore_ascii_case(x))
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn ends_with_ci(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// Path of a directory relative to the volume root (`"windows/temp"` for `C:\Windows\Temp`), if the
/// lower-cased `/`-separated `path` starts with a drive letter.
fn under_volume(path: &str) -> Option<&str> {
    let b = path.as_bytes();
    (b.len() > 3 && b[1] == b':' && b[2] == b'/').then(|| &path[3..])
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    /// Windows, Program Files, ProgramData, per-user installed programs, global npm packages:
    /// files there belong to installed software even when they look like build output or backups.
    in_system: bool,
    in_downloads: bool,
}

/// Per-user locations of installed software (suffixes of the lower-cased path).
const INSTALLED_SUFFIXES: &[&str] = &[
    "/appdata/local/programs",
    "/appdata/roaming/npm",
    "/appdata/local/microsoft",
];

struct Finder<'a> {
    tree: &'a Tree,
    now: u32,
    found: Vec<(Category, NodeId)>,
    /// Files worth checking for copies with the same content (see `crate::dupes`).
    dup_candidates: Vec<NodeId>,
}

impl Finder<'_> {
    fn has_marker(&self, dir: NodeId, marker: &str) -> bool {
        let Some(parent) = self.tree.parent(dir) else {
            return false;
        };
        self.tree
            .children(parent)
            .iter()
            .any(|&c| self.tree.node(c).kind == Kind::File && ends_with_ci(self.tree.name(c), marker))
    }

    /// A folder just called `cache` is only trusted near the top of an application's AppData folder
    /// (`AppData/Local/<app>/[<sub>/]cache`) or inside an embedded-browser profile; deeper ones are often
    /// application data that merely has that name (e.g. map tiles of a game mod).
    fn generic_cache_ok(&self, id: NodeId, path: &str) -> bool {
        let Some(i) = path.find("/appdata/") else { return false };
        let depth = path[i + "/appdata/".len()..].matches('/').count(); // local/<app>/cache -> 2
        if depth <= 3 || path.contains("/telegram desktop/tdata/") {
            return true;
        }
        let parent = self
            .tree
            .parent(id)
            .map(|p| self.tree.name(p).to_lowercase())
            .unwrap_or_default();
        let browser_profile = parent == "default" || parent.starts_with("profile ");
        (browser_profile
            && (path.contains("/user data/") || path.contains("/ebwebview/") || path.contains("/htmlcache/")))
            || parent.contains("webcache")
    }

    /// `path` is the lower-cased `/`-separated full path of `id`, `name` its lower-cased name.
    fn dir_rule(&self, id: NodeId, path: &str, name: &str, ctx: Ctx) -> Option<Category> {
        let rel = under_volume(path);
        if let Some(rel) = rel {
            if let Some(&(_, c)) = VOLUME_DIRS.iter().find(|(n, _)| *n == rel) {
                return Some(c);
            }
            if rel == "windows/temp" {
                return Some(Category::Temp);
            }
            if rel == "windows/minidump" || rel == "windows/livekernelreports" {
                return Some(Category::CrashDumps);
            }
            if rel == "windows/logs" {
                return Some(Category::Logs);
            }
        }
        if let Some(&(_, c)) = DIR_SUFFIXES.iter().find(|(s, _)| path.ends_with(s)) {
            return Some(c);
        }
        if BROWSER_CACHE_NAMES.contains(&name) && BROWSER_PROFILES.iter().any(|p| path.contains(p)) {
            return Some(Category::BrowserCache);
        }
        if path.contains("/appdata/") && !path.contains("/node_modules/") {
            if APP_CACHE_NAMES.contains(&name) {
                return Some(Category::AppCache);
            }
            if (name == "cache" || name == "caches" || name == "media_cache") && self.generic_cache_ok(id, path) {
                return Some(Category::AppCache);
            }
        }
        if name == ".cache" && rel.is_some_and(|r| r.starts_with("users/") && r.matches('/').count() == 2) {
            return Some(Category::AppCache);
        }
        if DEV_CACHE_NAMES.contains(&name) {
            return Some(Category::DevArtifacts);
        }
        if !ctx.in_system
            && !path.contains("/appdata/")
            && !path.contains("/extensions/")
            && !path.contains("/plugins/")
            && BUILD_DIRS.iter().any(|(d, m)| *d == name && self.has_marker(id, m))
        {
            return Some(Category::DevArtifacts);
        }
        None
    }

    fn file_rule(&self, id: NodeId, path: &str, ctx: Ctx) -> Option<Category> {
        let name = self.tree.name(id);
        let nd = self.tree.node(id);
        if ext_is(name, &["db"])
            && (starts_with_ci(name, "thumbcache_") || starts_with_ci(name, "iconcache_"))
            && path.ends_with("/microsoft/windows/explorer")
        {
            return Some(Category::Thumbnails);
        }
        if name.eq_ignore_ascii_case("docker_data.vhdx") || name.eq_ignore_ascii_case("ext4.vhdx") {
            return Some(Category::VirtualDisks);
        }
        if ext_is(name, &["dmp", "mdmp", "hdmp"]) {
            return Some(Category::CrashDumps);
        }
        if under_volume(path).is_none_or(|r| r.is_empty()) && name.eq_ignore_ascii_case("hiberfil.sys") {
            return Some(Category::Hibernation);
        }
        if ext_is(name, &["tmp", "temp"]) || (name.starts_with("~$") && nd.size < (4 << 20)) {
            return Some(Category::TempFiles);
        }
        if ext_is(name, &["log", "etl"]) || ends_with_ci(name, ".log.old") || is_rotated_log(name) {
            return Some(Category::Logs);
        }
        // Small *.bak files are typically save games or app-managed drafts: not worth the risk.
        if !ctx.in_system
            && nd.size >= (1 << 20)
            && !path.contains("/appdata/")
            && (ext_is(name, &["bak", "old"]) || (name.ends_with('~') && name.len() > 1))
        {
            return Some(Category::Backups);
        }
        let age = |min: u32| nd.mtime != 0 && self.now.saturating_sub(nd.mtime) > min;
        if ctx.in_downloads && ext_is(name, ARCHIVE_EXTS) && age(OLD_DOWNLOAD_AGE) {
            return Some(Category::OldDownloads);
        }
        if !ctx.in_system && nd.size >= BIG_OLD_SIZE && age(BIG_OLD_AGE) {
            return Some(Category::BigOld);
        }
        None
    }
}

/// `app.log.1`, `app.log.12`.
fn is_rotated_log(name: &str) -> bool {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() && name[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            ends_with_ci(&name[..i], ".log")
        }
        _ => false,
    }
}

/// Finds reclaimable files and folders. `now` is the current time in Unix seconds (for age rules).
/// Groups are ordered by safety, then by size; empty groups are omitted.
pub fn find(tree: &Tree, now: u32) -> Vec<Group> {
    find_with_dup_candidates(tree, now).0
}

/// Like [`find`], plus the files that may have content duplicates: pass them to `dupes::find`
/// (reading files is slow, so it is a separate step).
pub fn find_with_dup_candidates(tree: &Tree, now: u32) -> (Vec<Group>, Vec<NodeId>) {
    let mut f = Finder {
        tree,
        now,
        found: Vec::new(),
        dup_candidates: Vec::new(),
    };
    let mut root = tree.name(crate::tree::ROOT).to_lowercase().replace('\\', "/");
    while root.len() > 1 && root.ends_with('/') {
        root.pop();
    }

    let mut path = String::new();
    // (dir, length of the parent's path in `path`, context inherited from the parent)
    let mut stack: Vec<(NodeId, usize, Ctx)> = vec![(crate::tree::ROOT, 0, Ctx::default())];
    let mut name_lc = String::new();
    while let Some((id, plen, mut ctx)) = stack.pop() {
        path.truncate(plen);
        name_lc.clear();
        if id == crate::tree::ROOT {
            path.push_str(&root);
        } else {
            name_lc.extend(tree.name(id).chars().flat_map(char::to_lowercase));
            path.push('/');
            path.push_str(&name_lc);
            if let Some(c) = f.dir_rule(id, &path, &name_lc, ctx) {
                f.found.push((c, id));
                continue;
            }
        }
        if let Some(rel) = under_volume(&path) {
            if SKIP_UNDER_VOLUME.contains(&rel) {
                continue;
            }
            if SYSTEM_UNDER_VOLUME.contains(&rel) {
                ctx.in_system = true;
            }
            if rel.starts_with("users/") && rel.ends_with("/downloads") && rel.matches('/').count() == 2 {
                ctx.in_downloads = true;
            }
        }
        if INSTALLED_SUFFIXES.iter().any(|s| path.ends_with(s)) {
            ctx.in_system = true;
        }
        let l = path.len();
        for &c in tree.children(id) {
            match tree.node(c).kind {
                Kind::Dir => stack.push((c, l, ctx)),
                Kind::File => {
                    if let Some(cat) = f.file_rule(c, &path, ctx) {
                        f.found.push((cat, c));
                    } else if !ctx.in_system
                        && tree.node(c).size >= DUP_MIN_SIZE
                        && !path.contains("/appdata/")
                        && !path.contains("/plugins/")
                        && ext_is(tree.name(c), DUP_EXTS)
                    {
                        f.dup_candidates.push(c);
                    }
                }
                Kind::Link => {}
            }
        }
    }

    let mut groups: Vec<Group> = Category::ALL
        .iter()
        .map(|&category| Group {
            category,
            items: Vec::new(),
            size: 0,
            alloc: 0,
            files: 0,
        })
        .collect();
    for (cat, id) in f.found {
        let g = &mut groups[Category::ALL.iter().position(|&c| c == cat).unwrap()];
        let nd = tree.node(id);
        g.items.push(id);
        g.size += nd.size;
        g.alloc += nd.alloc;
        g.files += nd.files as u64;
    }
    groups.retain(|g| g.size >= MIN_GROUP);
    for g in &mut groups {
        g.items.sort_unstable_by_key(|&i| std::cmp::Reverse(tree.node(i).size));
        g.items.retain(|&i| tree.node(i).size > 0);
    }
    sort_groups(&mut groups);
    (groups, f.dup_candidates)
}

/// Orders groups by safety, then by size.
pub fn sort_groups(groups: &mut [Group]) {
    groups.sort_by(|a, b| a.category.safety().cmp(&b.category.safety()).then(b.size.cmp(&a.size)));
}

/// The "duplicates" group built from content-verified sets: every copy except the newest one of each set.
/// Copies deleted from the tree are ignored. Also returns, per offered copy, the copy that is kept.
pub fn duplicates_group(
    tree: &Tree,
    sets: &[crate::dupes::DupSet],
) -> Option<(Group, std::collections::HashMap<NodeId, NodeId>)> {
    let mut g = Group {
        category: Category::Duplicates,
        items: Vec::new(),
        size: 0,
        alloc: 0,
        files: 0,
    };
    let mut keep_of = std::collections::HashMap::new();
    for set in sets {
        let mut live = set.items.iter().copied().filter(|&i| !tree.is_removed(i));
        let Some(keep) = live.next() else { continue };
        for id in live {
            let nd = tree.node(id);
            g.items.push(id);
            g.size += nd.size;
            g.alloc += nd.alloc;
            g.files += 1;
            keep_of.insert(id, keep);
        }
    }
    g.items.sort_unstable_by_key(|&i| std::cmp::Reverse(tree.node(i).size));
    (!g.items.is_empty()).then_some((g, keep_of))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Builder, NO_PARENT};

    const NOW: u32 = 1_760_000_000;
    const M: u64 = 1 << 20;

    fn tree() -> Tree {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "C:\\", Kind::Dir, 0, 0);
        let users = b.add(root, "Users", Kind::Dir, 0, 0);
        let me = b.add(users, "me", Kind::Dir, 0, 0);
        let appdata = b.add(me, "AppData", Kind::Dir, 0, 0);
        let local = b.add(appdata, "Local", Kind::Dir, 0, 0);
        let temp = b.add(local, "Temp", Kind::Dir, 0, 0);
        b.add(temp, "x.tmp", Kind::File, 100 * M, 4096); // inside Temp: counted once, with the folder
        let chrome = b.add(local, "Google", Kind::Dir, 0, 0);
        let chrome = b.add(chrome, "Chrome", Kind::Dir, 0, 0);
        let ud = b.add(chrome, "User Data", Kind::Dir, 0, 0);
        let prof = b.add(ud, "Default", Kind::Dir, 0, 0);
        let cache = b.add(prof, "Cache", Kind::Dir, 0, 0);
        b.add(cache, "data_1", Kind::File, 500 * M, 4096);
        b.add(prof, "Bookmarks", Kind::File, 10 * M, 4096);
        let proj = b.add(me, "proj", Kind::Dir, 0, 0);
        b.add(proj, "package.json", Kind::File, 1, 1);
        let nm = b.add(proj, "node_modules", Kind::Dir, 0, 0);
        b.add(nm, "lib.js", Kind::File, 300 * M, 4096);
        let other = b.add(me, "docs", Kind::Dir, 0, 0);
        let fake_nm = b.add(other, "node_modules", Kind::Dir, 0, 0); // no package.json next to it
        b.add(fake_nm, "keep.txt", Kind::File, 7, 7);
        b.add(other, "report.docx.bak", Kind::File, 40 * M, 4096);
        b.add(other, "app.log.3", Kind::File, 20 * M, 4096);
        let dl = b.add(me, "Downloads", Kind::Dir, 0, 0);
        let old = b.add(dl, "setup.exe", Kind::File, 1000 * M, 4096);
        let new = b.add(dl, "fresh.zip", Kind::File, 900 * M, 4096);
        let big = b.add(other, "video.mkv", Kind::File, 2 << 30, 2 << 30);
        let win = b.add(root, "Windows", Kind::Dir, 0, 0);
        let sys_big = b.add(win, "big.dat", Kind::File, 3 << 30, 3 << 30);
        let wtemp = b.add(win, "Temp", Kind::Dir, 0, 0);
        b.add(wtemp, "a", Kind::File, 50 * M, 4096);
        let rb = b.add(root, "$Recycle.Bin", Kind::Dir, 0, 0);
        b.add(rb, "$R123.txt", Kind::File, 60 * M, 4096);
        b.add(root, "hiberfil.sys", Kind::File, 8000 * M, 8192);
        // installed software: never offered even though it looks like build output / backups
        let appdata_roaming = b.add(appdata, "Roaming", Kind::Dir, 0, 0);
        let npm = b.add(appdata_roaming, "npm", Kind::Dir, 0, 0);
        let gnm = b.add(npm, "node_modules", Kind::Dir, 0, 0);
        let tool = b.add(gnm, "tool", Kind::Dir, 0, 0);
        b.add(tool, "package.json", Kind::File, 1, 1);
        let tool_nm = b.add(tool, "node_modules", Kind::Dir, 0, 0);
        b.add(tool_nm, "dep.js", Kind::File, 70 * M, 4096);
        let pf = b.add(root, "Program Files", Kind::Dir, 0, 0);
        b.add(pf, "core.dll.bak", Kind::File, 90 * M, 4096);
        b.add(root, "tiny.tmp", Kind::File, 10, 4096); // below the group threshold
        let pics = b.add(me, "Pictures", Kind::Dir, 0, 0);
        let dup_old = b.add(pics, "Holiday.mp4", Kind::File, 600 * M, 4096);
        let dup_new = b.add(dl, "holiday.mp4", Kind::File, 600 * M, 4096);
        b.add(pics, "other.mp4", Kind::File, 601 * M, 4096);
        b.nodes[old as usize].mtime = NOW - 90 * DAY;
        b.nodes[new as usize].mtime = NOW - DAY;
        b.nodes[big as usize].mtime = NOW - 400 * DAY;
        b.nodes[sys_big as usize].mtime = NOW - 400 * DAY;
        b.nodes[dup_old as usize].mtime = NOW - 10 * DAY;
        b.nodes[dup_new as usize].mtime = NOW - DAY;
        b.finish()
    }

    fn sizes(t: &Tree) -> Vec<(Category, u64, Vec<String>)> {
        find(t, NOW)
            .into_iter()
            .map(|g| {
                (
                    g.category,
                    g.size,
                    g.items.iter().map(|&i| t.name(i).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn duplicates_group_offers_all_but_the_newest_and_skips_removed() {
        let mut t = tree();
        let (_, cands) = find_with_dup_candidates(&t, NOW);
        let names: Vec<&str> = cands.iter().map(|&i| t.name(i)).collect();
        assert!(names.contains(&"Holiday.mp4") && names.contains(&"holiday.mp4"));
        let id = |n: &str| *cands.iter().find(|&&i| t.name(i) == n).unwrap();
        let (old, new) = (id("Holiday.mp4"), id("holiday.mp4"));
        let set = crate::dupes::DupSet {
            size: 600 * M,
            items: vec![new, old],
        };
        let (g, keep) = duplicates_group(&t, std::slice::from_ref(&set)).unwrap();
        assert_eq!((g.items.clone(), g.size, keep[&old]), (vec![old], 600 * M, new));
        t.remove(old);
        assert!(duplicates_group(&t, &[set]).is_none(), "nothing left to offer");
    }

    #[test]
    fn finds_expected_categories_without_double_counting() {
        let t = tree();
        let got = sizes(&t);
        let get = |c: Category| got.iter().find(|g| g.0 == c).map(|g| (g.1, g.2.clone()));
        assert_eq!(get(Category::Temp), Some((150 * M, vec!["Temp".into(), "Temp".into()])));
        assert_eq!(get(Category::BrowserCache), Some((500 * M, vec!["Cache".into()])));
        assert_eq!(
            get(Category::DevArtifacts),
            Some((300 * M, vec!["node_modules".into()]))
        );
        assert_eq!(get(Category::Backups), Some((40 * M, vec!["report.docx.bak".into()])));
        assert_eq!(get(Category::Logs), Some((20 * M, vec!["app.log.3".into()])));
        assert_eq!(get(Category::OldDownloads), Some((1000 * M, vec!["setup.exe".into()])));
        assert_eq!(get(Category::BigOld), Some((2 << 30, vec!["video.mkv".into()])));
        assert_eq!(get(Category::RecycleBin).map(|g| g.0), Some(60 * M));
        assert_eq!(get(Category::Hibernation).map(|g| g.0), Some(8000 * M));
        assert_eq!(
            get(Category::Duplicates),
            None,
            "duplicates come from content hashing, not from names"
        );
        assert_eq!(
            get(Category::TempFiles),
            None,
            "x.tmp is inside Temp and must not be counted again"
        );
        // safest groups come first
        let order: Vec<Safety> = got.iter().map(|g| g.0.safety()).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn finds_ai_models_virtual_disks_and_driver_installers() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "C:\\", Kind::Dir, 0, 0);
        let me = b.add(root, "Users", Kind::Dir, 0, 0);
        let me = b.add(me, "me", Kind::Dir, 0, 0);
        let models = b.add(me, ".ollama", Kind::Dir, 0, 0);
        let models = b.add(models, "models", Kind::Dir, 0, 0);
        b.add(models, "blob", Kind::File, 5000 * M, 4096);
        let nuget = b.add(me, ".nuget", Kind::Dir, 0, 0);
        let nuget = b.add(nuget, "packages", Kind::Dir, 0, 0);
        b.add(nuget, "x.nupkg", Kind::File, 200 * M, 4096);
        let docker = b.add(me, "docker", Kind::Dir, 0, 0);
        b.add(docker, "Docker_Data.vhdx", Kind::File, 9000 * M, 9000 * M);
        let nv = b.add(root, "NVIDIA", Kind::Dir, 0, 0);
        b.add(nv, "setup.exe", Kind::File, 800 * M, 4096);
        let t = b.finish();
        let got = sizes(&t);
        let get = |c: Category| got.iter().find(|g| g.0 == c).map(|g| g.1);
        assert_eq!(get(Category::AiModels), Some(5000 * M));
        assert_eq!(get(Category::PackageCache), Some(200 * M));
        assert_eq!(get(Category::VirtualDisks), Some(9000 * M));
        assert_eq!(get(Category::DriverInstallers), Some(800 * M));
    }

    #[test]
    fn rotated_logs() {
        assert!(is_rotated_log("app.log.1"));
        assert!(is_rotated_log("X.LOG.12"));
        assert!(!is_rotated_log("app.1"));
        assert!(!is_rotated_log("log.1"));
    }
}
