//! `spacetree-cli --mcp`: SpaceTree as a Model Context Protocol server over stdin/stdout, so an AI agent in an
//! editor (Lumen Code, Claude Code...) can ask "why is drive C: running out of space" and get answers from a
//! real scan.
//!
//! Everything here is read-only: the tools scan, list, search and explain; none of them deletes, moves or
//! writes anything. Nothing goes to the network either — the server talks to the process that started it
//! and nothing else. Scans are kept in memory for the session, so asking several questions costs one scan.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spacetree_core::query::{self, Filter};
use spacetree_core::snapshot::{self, Snapshot};
use spacetree_core::stats::{format_date, human_size as human, type_stats};
use spacetree_core::tree::{Kind, NodeId, ROOT, Tree};
use spacetree_core::{Backend, Progress, ScanOutput, scan};

const PROTOCOL: &str = "2024-11-05";
/// A tool's answer is cut here: it goes into a model's context window.
const MAX_REPLY: usize = 24_000;
const MIN_GROWTH: u64 = 100 << 20;

struct Scanned {
    out: ScanOutput,
    taken: u64,
    /// The snapshot of the scan before this one (for "what grew").
    before: Option<Snapshot>,
}

#[derive(Default)]
struct Server {
    scans: HashMap<String, Scanned>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn root_key(path: &str) -> String {
    let mut p = path.trim().replace('/', "\\");
    if p.len() == 2 && p.ends_with(':') {
        p.push('\\');
    }
    p.to_lowercase()
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn dir_line(tree: &Tree, id: NodeId, total: u64) -> String {
    let n = tree.node(id);
    let pct = if total > 0 {
        n.size as f64 * 100.0 / total as f64
    } else {
        0.0
    };
    let kind = match n.kind {
        Kind::Dir => "папка",
        Kind::File => "файл",
        Kind::Link => "ссылка",
    };
    format!(
        "{:>10}  {:>5.1}%  {kind}  {}  (файлов: {}, изменено: {})",
        human(n.size),
        pct,
        tree.name(id),
        n.files,
        format_date(n.mtime)
    )
}

/// The node for `folder`: an absolute path inside the scanned root, or one relative to it.
fn resolve(tree: &Tree, folder: Option<&str>) -> Result<NodeId, String> {
    let Some(folder) = folder else { return Ok(ROOT) };
    let wanted = folder.trim().replace('/', "\\");
    let root = tree.path(ROOT);
    let rel = match wanted.to_lowercase().strip_prefix(&root.to_lowercase()) {
        Some(_) => wanted[root.len()..].to_owned(),
        None => wanted,
    };
    let mut cur = ROOT;
    for part in rel.split('\\').filter(|p| !p.is_empty()) {
        cur = tree
            .children(cur)
            .iter()
            .copied()
            .find(|&c| tree.name(c).eq_ignore_ascii_case(part))
            .ok_or_else(|| format!("в просканированном дереве нет «{folder}» (не найдено «{part}»)"))?;
    }
    Ok(cur)
}

impl Server {
    fn scanned(&mut self, root: &str, rescan: bool) -> Result<&Scanned, String> {
        let key = root_key(root);
        if rescan || !self.scans.contains_key(&key) {
            let path = PathBuf::from(if key.len() == 3 && key.ends_with(":\\") {
                &key
            } else {
                root.trim()
            });
            let out =
                scan(&path, Backend::Auto, &Progress::default()).map_err(|e| format!("ошибка сканирования: {e}"))?;
            let taken = now();
            let before = self
                .scans
                .remove(&key)
                .map(|s| Snapshot::from_tree(&s.out.tree, s.taken));
            self.scans.insert(key.clone(), Scanned { out, taken, before });
        }
        Ok(&self.scans[&key])
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<String, String> {
        let root = arg_str(args, "root").ok_or("нужен параметр root — диск или папка, например C:\\")?;
        match name {
            "scan" => {
                let s = self.scanned(root, args.get("rescan").and_then(Value::as_bool).unwrap_or(false))?;
                Ok(overview(s, root))
            }
            "list_folder" => {
                let s = self.scanned(root, false)?;
                let tree = &s.out.tree;
                let id = resolve(tree, arg_str(args, "folder"))?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(25).clamp(1, 200) as usize;
                let total = tree.node(id).size;
                let mut out = format!("{} — {}\n", tree.path(id), human(total));
                for &c in tree.children(id).iter().take(limit) {
                    out.push_str(&dir_line(tree, c, total));
                    out.push('\n');
                }
                Ok(out)
            }
            "find" => {
                let s = self.scanned(root, false)?;
                let tree = &s.out.tree;
                let filter = Filter::from_json(args);
                let hits = query::search(tree, &filter, now() as u32);
                let mut out = format!("найдено (показано до {}): {}\n", filter.limit, hits.len());
                for id in hits {
                    out.push_str(&format!(
                        "{:>10}  {}  {}\n",
                        human(tree.node(id).size),
                        format_date(tree.node(id).mtime),
                        tree.path(id)
                    ));
                }
                Ok(out)
            }
            "explain_folder" => {
                let s = self.scanned(root, false)?;
                let tree = &s.out.tree;
                let id = resolve(tree, arg_str(args, "folder"))?;
                Ok(query::folder_profile(tree, id))
            }
            "cleanup_candidates" => {
                let s = self.scanned(root, false)?;
                let tree = &s.out.tree;
                let groups = spacetree_core::cleanup::find(tree, now() as u32);
                let total: u64 = groups.iter().map(|g| g.size).sum();
                let mut out = format!("Можно освободить (оценка): {}\n", human(total));
                for g in &groups {
                    out.push_str(&format!(
                        "{:>10}  [{}] {} — объектов: {}\n",
                        human(g.size),
                        g.category.safety().label(),
                        g.category.title(),
                        g.items.len()
                    ));
                    for &id in g.items.iter().take(3) {
                        out.push_str(&format!("{:>10}      {}\n", human(tree.node(id).size), tree.path(id)));
                    }
                }
                Ok(out)
            }
            "what_grew" => {
                // A fresh scan against the previous one of this session; the first call only takes the baseline.
                let first = !self.scans.contains_key(&root_key(root));
                let s = self.scanned(root, !first)?;
                let Some(before) = &s.before else {
                    return Ok(
                        "Это первое сканирование в этой сессии — точка отсчёта сохранена. Вызовите what_grew позже, \
                               когда диск успеет измениться."
                            .into(),
                    );
                };
                let now_snap = Snapshot::from_tree(&s.out.tree, s.taken);
                let changes = snapshot::diff(before, &now_snap, MIN_GROWTH);
                let mut out = format!(
                    "С прошлого сканирования ({} мин назад) размер изменился на {}\n",
                    s.taken.saturating_sub(before.taken) / 60,
                    signed(now_snap.total as i64 - before.total as i64)
                );
                if changes.is_empty() {
                    out.push_str("Папок с изменением больше 100 МБ нет.\n");
                }
                for c in changes.iter().take(20) {
                    out.push_str(&format!("{:>12}  {}\n", signed(c.delta), c.path));
                }
                Ok(out)
            }
            _ => Err(format!("неизвестный инструмент: {name}")),
        }
    }
}

fn signed(n: i64) -> String {
    format!("{}{}", if n < 0 { "-" } else { "+" }, human(n.unsigned_abs()))
}

fn overview(s: &Scanned, root: &str) -> String {
    let tree = &s.out.tree;
    let (size, alloc, files) = tree.root_stats();
    let mut out = format!(
        "{root}: {} ({} на диске), файлов: {files}, папок: {}; сканирование {:.1} с ({}), ошибок доступа: {}\n",
        human(size),
        human(alloc),
        tree.dir_count().saturating_sub(1),
        s.out.elapsed.as_secs_f64(),
        s.out.selection.used.as_str(),
        s.out.errors
    );
    if let Some((total, free)) = spacetree_core::disk_space(std::path::Path::new(&tree.path(ROOT))) {
        out.push_str(&format!("Диск: всего {}, свободно {}\n", human(total), human(free)));
    }
    out.push_str("\nКрупнейшие папки верхнего уровня:\n");
    for &c in tree.children(ROOT).iter().take(15) {
        out.push_str(&dir_line(tree, c, size));
        out.push('\n');
    }
    out.push_str("\nТипы файлов:\n");
    for t in type_stats(tree, ROOT).iter().take(8) {
        out.push_str(&format!(
            "{:>10}  {}  ×{}\n",
            human(t.size),
            if t.ext.is_empty() {
                "(без расширения)"
            } else {
                &t.ext
            },
            t.count
        ));
    }
    out
}

fn tool(name: &str, description: &str, extra: Value) -> Value {
    let mut props = json!({
        "root": {"type": "string", "description": "Диск или папка, которую сканировать, например C:\\ или D:\\Games"}
    });
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": props, "required": ["root"]},
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false}
    })
}

fn tools() -> Value {
    let folder = json!({"folder": {"type": "string", "description": "Папка внутри root (полный или относительный путь); по умолчанию сам root"}});
    json!([
        tool(
            "scan",
            "Просканировать диск или папку и показать сводку: размер, свободное место, крупнейшие папки и типы файлов. \
             Результат кэшируется на время сессии.",
            json!({"rescan": {"type": "boolean", "description": "Сканировать заново, даже если результат уже есть"}})
        ),
        tool(
            "list_folder",
            "Содержимое папки по убыванию размера (долю от неё занимает каждый элемент).",
            {
                let mut f = folder.clone();
                f.as_object_mut().unwrap().insert(
                    "limit".into(),
                    json!({"type": "integer", "description": "Сколько строк, по умолчанию 25"}),
                );
                f
            }
        ),
        tool(
            "find",
            "Найти файлы (или папки, kind=dirs) по размеру, возрасту, расширению, имени и пути. Самые крупные — первыми.",
            json!({
                "min_size_mb": {"type": "number"}, "max_size_mb": {"type": "number"},
                "older_than_days": {"type": "number", "description": "Изменён не позже чем столько дней назад — то есть старше"},
                "newer_than_days": {"type": "number"},
                "extensions": {"type": "array", "items": {"type": "string"}},
                "name_contains": {"type": "array", "items": {"type": "string"}},
                "path_contains": {"type": "array", "items": {"type": "string"}},
                "exclude_path_contains": {"type": "array", "items": {"type": "string"}},
                "kind": {"type": "string", "enum": ["files", "dirs"]},
                "limit": {"type": "integer"}
            })
        ),
        tool(
            "explain_folder",
            "Факты о папке для ответа «что это такое и можно ли удалять»: путь, размер, типы файлов, крупнейшее внутри, даты.",
            folder
        ),
        tool(
            "cleanup_candidates",
            "Что обычно можно удалить: кэши, временные файлы, корзина, старые установщики — с оценкой безопасности. \
             Только список: удалять инструмент не умеет.",
            json!({})
        ),
        tool(
            "what_grew",
            "Что выросло с прошлого сканирования в этой сессии. Первый вызов сохраняет точку отсчёта, следующие \
             пересканируют диск и показывают папки, изменившиеся больше чем на 100 МБ.",
            json!({})
        ),
    ])
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_REPLY {
        let mut cut = MAX_REPLY;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n… (ответ обрезан)");
    }
    s
}

fn handle(server: &mut Server, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?; // a notification: no answer
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "spacetree", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "SpaceTree анализирует занятое место на диске. Все инструменты только читают. Начните с scan."
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let p = msg.get("params").cloned().unwrap_or(Value::Null);
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, is_error) = match server.call(name, &args) {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            };
            json!({"content": [{"type": "text", "text": truncate(text)}], "isError": is_error})
        }
        _ => {
            return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serves requests (one JSON object per line) until stdin closes.
pub fn run() {
    let mut server = Server::default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&mut server, &msg),
            Err(_) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})),
        };
        if let Some(r) = reply
            && (writeln!(stdout, "{r}").and_then(|()| stdout.flush())).is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(server: &mut Server, method: &str, params: Value) -> Value {
        handle(
            server,
            &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
        )
        .unwrap()
    }

    #[test]
    fn handshake_and_tool_list() {
        let mut s = Server::default();
        let r = rpc(&mut s, "initialize", json!({}));
        assert_eq!(r["result"]["serverInfo"]["name"], "spacetree");
        assert!(
            handle(
                &mut s,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none()
        );
        let r = rpc(&mut s, "tools/list", json!({}));
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "scan",
                "list_folder",
                "find",
                "explain_folder",
                "cleanup_candidates",
                "what_grew"
            ]
        );
        assert!(
            r["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["annotations"]["readOnlyHint"] == true)
        );
        assert_eq!(rpc(&mut s, "nope", json!({}))["error"]["code"], -32601);
    }

    #[test]
    fn scans_lists_finds_and_explains() {
        let dir = std::env::temp_dir().join(format!("spacetree-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Cache")).unwrap();
        std::fs::write(dir.join("Cache").join("a.bin"), vec![0u8; 3 << 20]).unwrap();
        std::fs::write(dir.join("b.zip"), vec![1u8; 2 << 20]).unwrap();
        let root = dir.to_str().unwrap();
        let mut s = Server::default();
        let call = |s: &mut Server, name: &str, args: Value| {
            let r = rpc(s, "tools/call", json!({"name": name, "arguments": args}));
            (
                r["result"]["content"][0]["text"].as_str().unwrap().to_owned(),
                r["result"]["isError"].as_bool().unwrap(),
            )
        };
        let (t, err) = call(&mut s, "scan", json!({"root": root}));
        assert!(!err && t.contains("Cache") && t.contains("b.zip"), "{t}");
        let (t, err) = call(&mut s, "list_folder", json!({"root": root, "folder": "Cache"}));
        assert!(!err && t.contains("a.bin"), "{t}");
        let (t, _) = call(
            &mut s,
            "find",
            json!({"root": root, "extensions": ["zip"], "min_size_mb": 1}),
        );
        assert!(t.contains("b.zip") && !t.contains("a.bin"), "{t}");
        let (t, _) = call(&mut s, "explain_folder", json!({"root": root, "folder": "Cache"}));
        assert!(t.contains("bin"), "{t}");
        let (t, err) = call(&mut s, "list_folder", json!({"root": root, "folder": "Missing"}));
        assert!(err && t.contains("Missing"), "{t}");
        let (t, err) = call(&mut s, "scan", json!({}));
        assert!(err && t.contains("root"));
        let (t, _) = call(&mut s, "what_grew", json!({"root": root}));
        assert!(
            t.contains("точка отсчёта") || t.contains("С прошлого сканирования"),
            "{t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
