//! Structured search over a scanned tree: the filter an AI model (or an MCP client) fills in when the user asks
//! "all archives over 500 MB that I downloaded more than half a year ago, except the game builds folder".
//! The model only produces data ([`Filter`], parsed by [`Filter::from_json`]); matching is plain code here, so
//! nothing a model says can run anything or touch the disk.

use serde_json::Value;

use crate::stats::ext_raw;
use crate::tree::{Kind, NodeId, Tree};

const DAY: u32 = 86_400;
/// Upper bound for `limit`; a result list is for a person (or a model's context), not for export.
pub const MAX_RESULTS: usize = 500;
const DEFAULT_RESULTS: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Last modified at least this many days ago (entries of unknown age never match).
    pub older_than_days: Option<u32>,
    /// Last modified at most this many days ago.
    pub newer_than_days: Option<u32>,
    /// Lower-case, without the dot. Empty = any.
    pub extensions: Vec<String>,
    /// Lower-case fragments; the name must contain at least one. Empty = any.
    pub name_contains: Vec<String>,
    /// Lower-case fragments; the full path must contain all of them.
    pub path_contains: Vec<String>,
    /// Lower-case fragments; a path containing any of them is skipped.
    pub exclude_path_contains: Vec<String>,
    /// Folders instead of files.
    pub dirs: bool,
    pub limit: usize,
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    let clean = |s: &str| s.trim().trim_start_matches('.').to_lowercase();
    match v.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(clean)
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) if !clean(s).is_empty() => vec![clean(s)],
        _ => Vec::new(),
    }
}

fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}

impl Filter {
    /// Reads a filter from JSON. Unknown keys are ignored, wrongly typed ones count as absent: a model's output
    /// is never trusted to be exact. Sizes are in megabytes (`min_size_mb`), ages in days.
    pub fn from_json(v: &Value) -> Filter {
        let mb = |key: &str| number(v, key).map(|n| (n * 1_048_576.0) as u64);
        let days = |key: &str| number(v, key).map(|n| n.min(f64::from(u32::MAX / DAY)) as u32);
        // directory separators are written either way
        let paths = |key: &str| -> Vec<String> { strings(v, key).into_iter().map(|p| p.replace('/', "\\")).collect() };
        Filter {
            min_size: mb("min_size_mb"),
            max_size: mb("max_size_mb"),
            older_than_days: days("older_than_days"),
            newer_than_days: days("newer_than_days"),
            extensions: strings(v, "extensions"),
            name_contains: strings(v, "name_contains"),
            path_contains: paths("path_contains"),
            exclude_path_contains: paths("exclude_path_contains"),
            dirs: v
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| k.eq_ignore_ascii_case("dirs")),
            limit: number(v, "limit")
                .map_or(DEFAULT_RESULTS, |n| n as usize)
                .clamp(1, MAX_RESULTS),
        }
    }

    fn needs_path(&self) -> bool {
        !self.path_contains.is_empty() || !self.exclude_path_contains.is_empty()
    }
}

/// Entries of `tree` that match `filter`, largest first. `now`: Unix seconds.
pub fn search(tree: &Tree, filter: &Filter, now: u32) -> Vec<NodeId> {
    let want = if filter.dirs { Kind::Dir } else { Kind::File };
    let mut hits: Vec<(u64, NodeId)> = Vec::new();
    for (i, n) in tree.nodes().iter().enumerate() {
        if n.removed || n.kind != want || i == 0 {
            continue;
        }
        if filter.min_size.is_some_and(|m| n.size < m) || filter.max_size.is_some_and(|m| n.size > m) {
            continue;
        }
        if let Some(d) = filter.older_than_days
            && (n.mtime == 0 || n.mtime > now.saturating_sub(d.saturating_mul(DAY)))
        {
            continue;
        }
        if let Some(d) = filter.newer_than_days
            && n.mtime < now.saturating_sub(d.saturating_mul(DAY))
        {
            continue;
        }
        let id = i as NodeId;
        let name = tree.name(id);
        if !filter.extensions.is_empty() && !filter.extensions.iter().any(|e| ext_raw(name).eq_ignore_ascii_case(e)) {
            continue;
        }
        if !filter.name_contains.is_empty() {
            let lower = name.to_lowercase();
            if !filter.name_contains.iter().any(|f| lower.contains(f.as_str())) {
                continue;
            }
        }
        if filter.needs_path() {
            let path = tree.path(id).to_lowercase();
            if !filter.path_contains.iter().all(|f| path.contains(f.as_str()))
                || filter.exclude_path_contains.iter().any(|f| path.contains(f.as_str()))
            {
                continue;
            }
        }
        hits.push((n.size, id));
    }
    hits.sort_unstable_by(|a, b| b.cmp(a));
    let limit = if filter.limit == 0 {
        DEFAULT_RESULTS
    } else {
        filter.limit.min(MAX_RESULTS)
    };
    hits.truncate(limit);
    hits.into_iter().map(|(_, id)| id).collect()
}

/// Facts about one folder that tell what it is: its name and place, size, the extensions that fill it, the
/// biggest entries directly inside and the dates. This is everything a model gets to see about a folder —
/// names and numbers, never file contents.
pub fn folder_profile(tree: &Tree, id: NodeId) -> String {
    use crate::stats::{format_date, human_size, type_stats};
    let n = tree.node(id);
    let mut out = format!(
        "Папка: {}\nРазмер: {} · файлов: {} · последнее изменение внутри: {}\n",
        tree.path(id),
        human_size(n.size),
        n.files,
        format_date(n.mtime)
    );
    out.push_str("Типы файлов (по размеру):");
    for t in type_stats(tree, id).iter().take(8) {
        let ext = if t.ext.is_empty() {
            "(без расширения)"
        } else {
            t.ext.as_str()
        };
        out.push_str(&format!(" {ext} {} ×{};", human_size(t.size), t.count));
    }
    out.push_str("\nКрупнейшее внутри:\n");
    for &c in tree.children(id).iter().take(12) {
        let cn = tree.node(c);
        let kind = if cn.kind == Kind::Dir { "папка" } else { "файл" };
        out.push_str(&format!(
            "  {kind} {} · {} · {}\n",
            tree.name(c),
            human_size(cn.size),
            format_date(cn.mtime)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Builder, NO_PARENT};

    const MB: u64 = 1 << 20;
    const NOW: u32 = 1_000_000_000;

    fn tree() -> Tree {
        let mut b = Builder::default();
        let root = b.add(NO_PARENT, "D:\\", Kind::Dir, 0, 0);
        let dl = b.add(root, "Downloads", Kind::Dir, 0, 0);
        let games = b.add(root, "GameBuilds", Kind::Dir, 0, 0);
        for (parent, name, size, age_days) in [
            (dl, "old.zip", 900 * MB, 400),
            (dl, "new.zip", 900 * MB, 3),
            (dl, "small.zip", 10 * MB, 400),
            (dl, "setup.exe", 600 * MB, 300),
            (dl, "photo.jpg", 800 * MB, 400),
            (games, "build.zip", 2000 * MB, 400),
        ] {
            let id = b.add(parent, name, Kind::File, size, size);
            b.nodes[id as usize].mtime = NOW - age_days * DAY;
        }
        b.finish()
    }

    fn names(t: &Tree, ids: &[NodeId]) -> Vec<String> {
        ids.iter().map(|&i| t.name(i).to_owned()).collect()
    }

    #[test]
    fn old_big_archives_and_installers_except_a_folder() {
        let t = tree();
        let f = Filter::from_json(&serde_json::json!({
            "min_size_mb": 500,
            "older_than_days": 180,
            "extensions": ["zip", ".EXE"],
            "exclude_path_contains": ["GameBuilds"],
        }));
        assert_eq!(names(&t, &search(&t, &f, NOW)), ["old.zip", "setup.exe"]);
    }

    #[test]
    fn newer_than_and_limit() {
        let t = tree();
        let f = Filter::from_json(&serde_json::json!({"newer_than_days": 30, "limit": 1}));
        assert_eq!(names(&t, &search(&t, &f, NOW)), ["new.zip"]);
        let all = Filter::from_json(&serde_json::json!({"limit": 3}));
        assert_eq!(search(&t, &all, NOW).len(), 3);
    }

    #[test]
    fn garbage_from_a_model_is_harmless() {
        let t = tree();
        let f = Filter::from_json(&serde_json::json!({
            "min_size_mb": "big", "older_than_days": -5, "extensions": 7, "limit": 1e12, "weird": {"a": 1},
            "path_contains": "downloads/",
        }));
        assert_eq!(f.min_size, None);
        assert_eq!(f.older_than_days, None);
        assert!(f.extensions.is_empty());
        assert_eq!(f.limit, MAX_RESULTS);
        assert_eq!(f.path_contains, ["downloads\\"]);
        assert_eq!(search(&t, &f, NOW).len(), 5);
    }

    #[test]
    fn folders_can_be_searched_and_profiled() {
        let t = tree();
        let f = Filter::from_json(&serde_json::json!({"kind": "dirs", "min_size_mb": 2500}));
        assert_eq!(names(&t, &search(&t, &f, NOW)), ["Downloads"]);
        let id = search(&t, &f, NOW)[0];
        let p = folder_profile(&t, id);
        assert!(p.contains("Downloads") && p.contains("zip"), "{p}");
    }
}
