//! Leftovers of programs that are gone: big folders in `AppData` that match no installed program, and game
//! folders of Steam / Epic Games that their launcher no longer knows about.
//!
//! Everything here is a *hint*: folders are reported as "review", never selected for the user. Matching is by
//! names (the program list comes from the registry and the Program Files folders), so portable programs and
//! folders named after a vendor or a technology can look orphaned when they are not.

use std::collections::HashSet;
use std::path::Path;

use crate::cleanup::{Category, Group};
use crate::tree::{Kind, NodeId, ROOT, Tree};

/// Smaller folders are not worth a line in the report.
pub const ORPHAN_MIN: u64 = 50 << 20;
/// How deep below the scan root `AppData` / `steamapps` / `Epic Games` folders are looked for.
const SEARCH_DEPTH: u32 = 4;
/// Shorter names must match exactly; a 3-letter folder is not "contained" in a program name by accident.
const MIN_FUZZY: usize = 4;

/// `AppData` folders that belong to Windows, to shared runtimes or to something the cleanup tab already
/// handles; whatever program list is available, these are not "orphans".
const SYSTEM_APPDATA: &[&str] = &[
    "microsoft",
    "packages",
    "temp",
    "programs",
    "d3dscache",
    "crashdumps",
    "connecteddevicesplatform",
    "windowsapps",
    "publishers",
    "virtualstore",
    "comms",
    "history",
    "placeholdertilelogger",
    "diagnostics",
    "peerdistrepub",
    "temporary internet files",
    "application data",
    "tmp",
    "nvidia",
    "nvidia corporation",
    "amd",
    "intel",
    "pip",
    "npm",
    "nuget",
    "cache",
    "caches",
    // developer tooling that downloads its own browsers, runtimes and package stores
    "ms-playwright",
    "ms-playwright-go",
    "electron",
    "vcpkg",
    "node-gyp",
    "yarn",
    "pnpm",
    "go-build",
    "puppeteer",
    "cypress",
];

/// Epic Games folders that are not games.
const SYSTEM_EPIC: &[&str] = &[
    "launcher",
    "epiconlineservices",
    "directxredist",
    "portal",
    "epicgameslauncher",
];

/// Names of what is installed, normalised for comparison (see [`norm`]).
#[derive(Clone, Debug, Default)]
pub struct Installed {
    tokens: Vec<String>,
}

impl Installed {
    /// `names`: display names, publishers, install folder names, shortcut names — anything that identifies a
    /// program. Empty strings and names with nothing to compare are dropped.
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        let mut tokens: Vec<String> = names.into_iter().map(|n| norm(&n)).filter(|n| !n.is_empty()).collect();
        tokens.sort();
        tokens.dedup();
        Installed { tokens }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the (normalised) folder name is the name of, or part of the name of, an installed program or
    /// the other way round.
    fn knows(&self, folder: &str) -> bool {
        if folder.is_empty() {
            return true;
        }
        self.tokens.iter().any(|t| {
            t == folder
                || (folder.chars().count() >= MIN_FUZZY && t.contains(folder))
                || (t.chars().count() >= MIN_FUZZY && folder.contains(t.as_str()))
        })
    }
}

/// Lower-case letters and digits only: `"Zoom Video Communications, Inc."` -> `"zoomvideocommunicationsinc"`.
pub fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn child_named(tree: &Tree, id: NodeId, name: &str) -> Option<NodeId> {
    tree.children(id)
        .iter()
        .copied()
        .find(|&c| tree.is_dir(c) && tree.name(c).eq_ignore_ascii_case(name))
}

/// Directories named `name` (any case) within `SEARCH_DEPTH` levels below the scan root, root included.
fn find_dirs(tree: &Tree, name: &str) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut level = vec![ROOT];
    if tree
        .name(ROOT)
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .is_some_and(|n| n.eq_ignore_ascii_case(name))
    {
        out.push(ROOT);
    }
    for _ in 0..SEARCH_DEPTH {
        let mut next = Vec::new();
        for &d in &level {
            for &c in tree.children(d) {
                if !tree.is_dir(c) {
                    continue;
                }
                if tree.name(c).eq_ignore_ascii_case(name) {
                    out.push(c);
                } else {
                    next.push(c);
                }
            }
        }
        level = next;
    }
    out
}

/// Names of the program folders the tree itself shows: `Program Files`, `Program Files (x86)` and the per-user
/// `AppData\Local\Programs`. They complete the registry's list (portable and manually unpacked programs).
pub fn program_folder_names(tree: &Tree) -> Vec<String> {
    let mut out = Vec::new();
    let mut add_children = |d: NodeId| {
        out.extend(
            tree.children(d)
                .iter()
                .filter(|&&c| tree.is_dir(c))
                .map(|&c| tree.name(c).to_owned()),
        );
    };
    for pf in ["Program Files", "Program Files (x86)"] {
        find_dirs(tree, pf).into_iter().for_each(&mut add_children);
    }
    for programs in find_dirs(tree, "Programs") {
        let in_local = tree
            .parent(programs)
            .is_some_and(|p| tree.name(p).eq_ignore_ascii_case("local"));
        if in_local {
            add_children(programs);
        }
    }
    out
}

fn dir_group(tree: &Tree, category: Category, mut items: Vec<NodeId>, claimed: &[NodeId]) -> Option<Group> {
    items.sort_by_key(|&i| std::cmp::Reverse(tree.node(i).size));
    items.dedup();
    if items.is_empty() {
        return None;
    }
    // Space that other cleanup groups already count inside these folders is not counted a second time.
    let set: HashSet<NodeId> = items.iter().copied().collect();
    let mut overlap = 0u64;
    let mut overlap_alloc = 0u64;
    for &c in claimed {
        if tree.ancestors(c).iter().rev().skip(1).any(|a| set.contains(a)) {
            overlap = overlap.saturating_add(tree.node(c).size);
            overlap_alloc = overlap_alloc.saturating_add(tree.node(c).alloc);
        }
    }
    let sum = |f: fn(&crate::tree::Node) -> u64| items.iter().fold(0u64, |a, &i| a.saturating_add(f(tree.node(i))));
    Some(Group {
        category,
        size: sum(|n| n.size).saturating_sub(overlap),
        alloc: sum(|n| n.alloc).saturating_sub(overlap_alloc),
        files: items.iter().map(|&i| u64::from(tree.node(i).files)).sum(),
        items,
    })
}

/// A folder that holds a program (an `.exe` in it or one level below it, as per-user installers like Squirrel lay
/// them out) is an installation, not leftover data, whatever it is called.
fn holds_program(tree: &Tree, dir: NodeId) -> bool {
    let is_exe = |c: NodeId| tree.node(c).kind == Kind::File && tree.name(c).to_ascii_lowercase().ends_with(".exe");
    tree.children(dir)
        .iter()
        .any(|&c| is_exe(c) || (tree.is_dir(c) && tree.children(c).iter().any(|&g| is_exe(g))))
}

/// Big `AppData\{Roaming,Local,LocalLow}` folders that match no installed program. Folders another cleanup group
/// already offers (an npm cache, a browser profile's cache...) are that group's business, not orphans.
fn appdata_orphans(tree: &Tree, installed: &Installed, offered: &HashSet<NodeId>) -> Vec<NodeId> {
    let mut out = Vec::new();
    for appdata in find_dirs(tree, "AppData") {
        for sub in ["Roaming", "Local", "LocalLow"] {
            let Some(base) = child_named(tree, appdata, sub) else {
                continue;
            };
            for &c in tree.children(base) {
                let name = tree.name(c);
                let known = SYSTEM_APPDATA.iter().any(|s| name.eq_ignore_ascii_case(s));
                if tree.is_dir(c)
                    && tree.node(c).size >= ORPHAN_MIN
                    && !known
                    && !offered.contains(&c)
                    && !norm(name).contains("cache")
                    && !installed.knows(&norm(name))
                    && !holds_program(tree, c)
                {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// The value of `"key"  "value"` in a Valve KeyValues file (`appmanifest_*.acf`, `libraryfolders.vdf`).
pub fn vdf_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split('"').skip(1).step_by(2);
        (parts.next()?.eq_ignore_ascii_case(key)).then(|| parts.next().map(str::to_owned))?
    })
}

/// What a Steam library's manifests say is installed: `(install folder names, app ids)`, both lower-case.
/// `None` when the library has no readable manifest (then nothing can be judged).
fn steam_library(steamapps: &Path) -> Option<(HashSet<String>, HashSet<String>)> {
    let (mut dirs, mut ids) = (HashSet::new(), HashSet::new());
    for e in std::fs::read_dir(steamapps).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        if let Some(dir) = vdf_value(&text, "installdir") {
            dirs.insert(dir.to_lowercase());
        }
        if let Some(id) = vdf_value(&text, "appid") {
            ids.insert(id);
        }
    }
    (!ids.is_empty()).then_some((dirs, ids))
}

/// Game folders and Proton/shader leftovers of Steam games that are no longer installed.
fn steam_orphans(tree: &Tree) -> Vec<NodeId> {
    let mut out = Vec::new();
    for steamapps in find_dirs(tree, "steamapps") {
        let Some((dirs, ids)) = steam_library(&crate::fs_path(&tree.path(steamapps))) else {
            continue;
        };
        let check = |folder: &str, f: &dyn Fn(&str) -> bool, out: &mut Vec<NodeId>| {
            if let Some(d) = child_named(tree, steamapps, folder) {
                out.extend(
                    tree.children(d)
                        .iter()
                        .copied()
                        .filter(|&c| tree.is_dir(c) && tree.node(c).size >= ORPHAN_MIN && !f(tree.name(c))),
                );
            }
        };
        check("common", &|n| dirs.contains(&n.to_lowercase()), &mut out);
        check("compatdata", &|n| ids.contains(n), &mut out);
        check("shadercache", &|n| ids.contains(n), &mut out);
    }
    out
}

/// Every `"InstallLocation"` of an Epic Games Launcher manifest (`*.item`, JSON), unescaped and normalised.
pub fn epic_location(text: &str) -> Option<String> {
    let at = text.find("\"InstallLocation\"")? + "\"InstallLocation\"".len();
    let rest = text[at..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(normalise_path(&out)),
            '\\' => match chars.next()? {
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                '"' => out.push('"'),
                'n' | 't' | 'r' => return None, // not a path
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

fn normalise_path(p: &str) -> String {
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Locations of games the Epic Games Launcher knows. `None` when its manifest folder is missing or empty.
fn epic_known(manifests: &Path) -> Option<HashSet<String>> {
    let set: HashSet<String> = std::fs::read_dir(manifests)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")))
        .filter_map(|e| epic_location(&std::fs::read_to_string(e.path()).ok()?))
        .collect();
    (!set.is_empty()).then_some(set)
}

fn epic_manifest_dir() -> Option<std::path::PathBuf> {
    let pd = std::env::var_os("ProgramData")?;
    Some(Path::new(&pd).join(r"Epic\EpicGamesLauncher\Data\Manifests"))
}

fn epic_orphans(tree: &Tree, known: &HashSet<String>) -> Vec<NodeId> {
    let mut out = Vec::new();
    for games in find_dirs(tree, "Epic Games") {
        for &c in tree.children(games) {
            let name = tree.name(c);
            if !tree.is_dir(c)
                || tree.node(c).size < ORPHAN_MIN
                || SYSTEM_EPIC.iter().any(|s| norm(name) == *s)
                || known.contains(&normalise_path(&tree.path(c)))
            {
                continue;
            }
            out.push(c);
        }
    }
    out
}

/// Finds the orphaned-data groups. `claimed` are the items of the other cleanup groups: their space inside an
/// orphaned folder is not counted twice. An empty `installed` list switches the AppData check off (nothing
/// can be judged without it); the game checks only need the launchers' own manifests.
pub fn find(tree: &Tree, installed: &Installed, claimed: &[NodeId]) -> Vec<Group> {
    find_with(
        tree,
        installed,
        epic_manifest_dir().and_then(|d| epic_known(&d)).as_ref(),
        claimed,
    )
}

fn find_with(tree: &Tree, installed: &Installed, epic: Option<&HashSet<String>>, claimed: &[NodeId]) -> Vec<Group> {
    let mut out = Vec::new();
    if !installed.is_empty() {
        // the programs visible in the tree itself count as installed too
        let from_tree = Installed::new(program_folder_names(tree));
        let all = Installed::new(installed.tokens.iter().cloned().chain(from_tree.tokens));
        let offered: HashSet<NodeId> = claimed.iter().copied().collect();
        out.extend(dir_group(
            tree,
            Category::OrphanedData,
            appdata_orphans(tree, &all, &offered),
            claimed,
        ));
    }
    let mut games = steam_orphans(tree);
    if let Some(known) = epic {
        games.extend(epic_orphans(tree, known));
    }
    games.retain(|&i| tree.node(i).kind == Kind::Dir);
    out.extend(dir_group(tree, Category::OrphanedGames, games, claimed));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Builder, NO_PARENT};

    const MB: u64 = 1 << 20;

    #[test]
    fn names_are_normalised() {
        assert_eq!(norm("Zoom Video Communications, Inc."), "zoomvideocommunicationsinc");
        assert_eq!(norm("  "), "");
        assert_eq!(norm("Ünïcode-App_2"), "ünïcodeapp2");
    }

    #[test]
    fn matching_is_tolerant_but_not_for_tiny_names() {
        let inst = Installed::new([
            "Zoom Video Communications, Inc.".to_owned(),
            "Git".to_owned(),
            String::new(),
        ]);
        assert!(inst.knows("zoom"));
        assert!(inst.knows("git"));
        assert!(!inst.knows("gi"), "short names need an exact match");
        assert!(
            inst.knows("zoomvideocommunicationsincupdater"),
            "a longer folder that contains a program name"
        );
        assert!(!inst.knows("deadapp"));
        assert_eq!(inst.len(), 2);
    }

    #[test]
    fn folders_with_a_program_inside_are_installations() {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, r"C:\", Kind::Dir, 0, 0);
        let users = b.add(root, "Users", Kind::Dir, 0, 0);
        let bob = b.add(users, "bob", Kind::Dir, 0, 0);
        let ad = b.add(bob, "AppData", Kind::Dir, 0, 0);
        let local = b.add(ad, "Local", Kind::Dir, 0, 0);
        let portable = b.add(local, "Tool", Kind::Dir, 0, 0);
        b.add(portable, "tool.exe", Kind::File, 100 * MB, 100 * MB);
        let squirrel = b.add(local, "Chatty", Kind::Dir, 0, 0);
        let ver = b.add(squirrel, "app-1.2.3", Kind::Dir, 0, 0);
        b.add(ver, "chatty.EXE", Kind::File, 100 * MB, 100 * MB);
        let data = b.add(local, "LeftoverData", Kind::Dir, 0, 0);
        b.add(data, "blob.db", Kind::File, 100 * MB, 100 * MB);
        let cache = b.add(local, "Some Package Cache", Kind::Dir, 0, 0);
        b.add(cache, "x.bin", Kind::File, 100 * MB, 100 * MB);
        let tree = b.finish();
        let groups = find_with(&tree, &Installed::new(["Zoom".to_owned()]), None, &[]);
        let names: Vec<&str> = groups[0].items.iter().map(|&i| tree.name(i)).collect();
        assert_eq!(names, ["LeftoverData"]);
    }

    fn appdata_tree() -> Tree {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, r"C:\", Kind::Dir, 0, 0);
        let users = b.add(root, "Users", Kind::Dir, 0, 0);
        let bob = b.add(users, "bob", Kind::Dir, 0, 0);
        let ad = b.add(bob, "AppData", Kind::Dir, 0, 0);
        let roaming = b.add(ad, "Roaming", Kind::Dir, 0, 0);
        let local = b.add(ad, "Local", Kind::Dir, 0, 0);
        for (parent, name, size) in [
            (roaming, "DeadApp", 200 * MB),
            (roaming, "Zoom", 300 * MB),
            (roaming, "Microsoft", 900 * MB),
            (roaming, "TinyLeft", MB),
            (local, "GhostEditor", 80 * MB),
            (local, "PortableTool", 70 * MB),
        ] {
            let d = b.add(parent, name, Kind::Dir, 0, 0);
            b.add(d, "data.bin", Kind::File, size, size);
        }
        let pf = b.add(root, "Program Files", Kind::Dir, 0, 0);
        b.add(pf, "PortableTool", Kind::Dir, 0, 0);
        b.finish()
    }

    #[test]
    fn appdata_folders_without_a_program_are_orphans() {
        let tree = appdata_tree();
        let installed = Installed::new(["Zoom Video Communications, Inc.".to_owned()]);
        let groups = find_with(&tree, &installed, None, &[]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.category, Category::OrphanedData);
        let names: Vec<&str> = g.items.iter().map(|&i| tree.name(i)).collect();
        assert_eq!(
            names,
            ["DeadApp", "GhostEditor"],
            "Zoom is installed, PortableTool sits in Program Files"
        );
        assert_eq!(g.size, 280 * MB);
    }

    #[test]
    fn folders_that_another_group_already_offers_are_not_orphans() {
        let tree = appdata_tree();
        let installed = Installed::new(["Zoom".to_owned()]);
        let ghost = (0..tree.len() as NodeId)
            .find(|&i| tree.name(i) == "GhostEditor")
            .unwrap();
        let groups = find_with(&tree, &installed, None, &[ghost]);
        let names: Vec<&str> = groups[0].items.iter().map(|&i| tree.name(i)).collect();
        assert_eq!(names, ["DeadApp"]);
    }

    #[test]
    fn without_a_program_list_appdata_is_not_judged() {
        let tree = appdata_tree();
        assert!(find_with(&tree, &Installed::default(), None, &[]).is_empty());
    }

    #[test]
    fn space_counted_by_other_groups_is_not_counted_twice() {
        let tree = appdata_tree();
        let installed = Installed::new(["Zoom".to_owned()]);
        let dead = (0..tree.len() as NodeId).find(|&i| tree.name(i) == "DeadApp").unwrap();
        let cached = tree.children(dead)[0];
        let groups = find_with(&tree, &installed, None, &[cached]);
        assert_eq!(groups[0].size, 80 * MB, "DeadApp's only file was claimed elsewhere");
        assert_eq!(groups[0].items.len(), 2);
    }

    #[test]
    fn parses_vdf_and_epic_manifests() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"570\"\n\t\"installdir\"\t\t\"dota 2 beta\"\n}\n";
        assert_eq!(vdf_value(acf, "appid").as_deref(), Some("570"));
        assert_eq!(vdf_value(acf, "installdir").as_deref(), Some("dota 2 beta"));
        assert_eq!(vdf_value(acf, "missing"), None);
        let item = r#"{ "FormatVersion": 0, "InstallLocation": "D:\\Games\\Epic Games\\Foo\\", "x": 1 }"#;
        assert_eq!(epic_location(item).as_deref(), Some(r"d:\games\epic games\foo"));
        assert_eq!(
            epic_location(r#"{"InstallLocation": "C:\u0041\\x"}"#).as_deref(),
            Some(r"c:a\x")
        );
        assert_eq!(epic_location("{}"), None);
        assert_eq!(epic_location(r#"{"InstallLocation": "unterminated"#), None);
    }

    #[test]
    fn steam_and_epic_leftovers_are_found_through_the_launchers_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let steamapps = dir.path().join("SteamLibrary").join("steamapps");
        std::fs::create_dir_all(steamapps.join("common").join("Alive")).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_10.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\"10\"\n\t\"installdir\"\t\"Alive\"\n}\n",
        )
        .unwrap();

        let mut b = Builder::default();
        let root = b.add(NO_PARENT, dir.path().to_str().unwrap(), Kind::Dir, 0, 0);
        let lib = b.add(root, "SteamLibrary", Kind::Dir, 0, 0);
        let sa = b.add(lib, "steamapps", Kind::Dir, 0, 0);
        let common = b.add(sa, "common", Kind::Dir, 0, 0);
        let compat = b.add(sa, "compatdata", Kind::Dir, 0, 0);
        for (parent, name) in [
            (common, "Alive"),
            (common, "Removed Game"),
            (compat, "10"),
            (compat, "999"),
        ] {
            let d = b.add(parent, name, Kind::Dir, 0, 0);
            b.add(d, "f", Kind::File, 100 * MB, 100 * MB);
        }
        let epic = b.add(root, "Epic Games", Kind::Dir, 0, 0);
        for name in ["Fortnite", "Old Epic Game", "Launcher"] {
            let d = b.add(epic, name, Kind::Dir, 0, 0);
            b.add(d, "f", Kind::File, 100 * MB, 100 * MB);
        }
        let tree = b.finish();

        let known: HashSet<String> = [normalise_path(&format!(
            r"{}\Epic Games\Fortnite",
            dir.path().display()
        ))]
        .into();
        let groups = find_with(&tree, &Installed::default(), Some(&known), &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].category, Category::OrphanedGames);
        let mut names: Vec<String> = groups[0].items.iter().map(|&i| tree.name(i).to_owned()).collect();
        names.sort();
        assert_eq!(names, ["999", "Old Epic Game", "Removed Game"]);
    }

    #[test]
    fn a_steam_library_without_manifests_is_not_judged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steamapps").join("common").join("Mystery")).unwrap();
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, dir.path().to_str().unwrap(), Kind::Dir, 0, 0);
        let sa = b.add(root, "steamapps", Kind::Dir, 0, 0);
        let common = b.add(sa, "common", Kind::Dir, 0, 0);
        let d = b.add(common, "Mystery", Kind::Dir, 0, 0);
        b.add(d, "f", Kind::File, 100 * MB, 100 * MB);
        assert!(find_with(&b.finish(), &Installed::default(), None, &[]).is_empty());
    }
}
