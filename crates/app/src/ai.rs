//! Optional AI helper: "what is this folder?", search in plain words, "why did the disk grow".
//!
//! Off by default. When switched on it talks to a local Ollama (`/api/chat`) and to nothing else: the address
//! must be a loopback one, proxies and redirects are disabled, so no folder name can leave the computer through
//! this module. What the model gets is names, sizes and dates (never file contents), and what it returns is
//! only displayed — or, for search, a [`Filter`] that plain code in `spacetree_core::query` evaluates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use eframe::egui::{self, Align2, RichText, vec2};
use serde_json::{Value, json};
use spacetree_core::NodeId;
use spacetree_core::query::{self, Filter};
use spacetree_core::stats::human_size;

use crate::{App, ToastKind, now_unix};

pub const DEFAULT_URL: &str = "http://127.0.0.1:11434";
const TIMEOUT: Duration = Duration::from_secs(180);
/// A model's answer is cut here; a runaway generation must not fill the window or memory.
const MAX_ANSWER: u64 = 256 * 1024;
const GROWTH_ROWS: usize = 15;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether the AI helper is switched on (kept in step with the settings every frame; menus read it).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The address is a loopback one: `localhost`, `127.x.x.x` or `[::1]`, over plain http, without credentials.
pub fn is_local_url(url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    match host.to_ascii_lowercase().as_str() {
        "localhost" | "::1" => true,
        h => h.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback()),
    }
}

#[derive(Clone)]
pub struct Config {
    /// Claude Code (cloud) instead of Ollama (local).
    pub claude: bool,
    pub url: String,
    pub model: String,
}

/// One synchronous round-trip to the chosen backend.
fn chat(cfg: &Config, system: &str, user: &str, json_mode: bool) -> Result<String, String> {
    if cfg.claude {
        chat_claude(system, user)
    } else {
        chat_ollama(cfg, system, user, json_mode)
    }
}

/// Asks Claude Code (`claude -p`, the user's own subscription) in a locked-down one-shot session: no tools, no
/// MCP servers, no saved session, run from an empty temp folder so no project instructions are picked up. The
/// question travels over stdin, the reply is plain text. THIS SENDS the names, sizes and dates to Anthropic.
fn chat_claude(system: &str, user: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--tools", "", "--strict-mcp-config", "--disable-slash-commands"])
        .arg("--no-session-persistence")
        .args(["--system-prompt", system])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| {
        format!("не удалось запустить claude ({e}). Нужен установленный Claude Code (claude.exe в PATH) с выполненным входом")
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(user.as_bytes()); // dropped at the end of the block: EOF for the child
    }
    // drain the pipes on threads so a chatty child cannot block on a full pipe while we wait
    let mut out = child.stdout.take().ok_or("нет stdout")?;
    let mut err = child.stderr.take().ok_or("нет stderr")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = (&mut out).take(MAX_ANSWER).read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = (&mut err).take(8192).read_to_string(&mut s);
        s
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("claude не ответил вовремя".into());
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let (text, errors) = (reader.join().unwrap_or_default(), err_reader.join().unwrap_or_default());
    if !status.success() {
        return Err(format!(
            "claude завершился с ошибкой: {}",
            errors.trim().chars().take(200).collect::<String>()
        ));
    }
    Some(text.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "claude вернул пустой ответ".into())
}

fn chat_ollama(cfg: &Config, system: &str, user: &str, json_mode: bool) -> Result<String, String> {
    if !is_local_url(&cfg.url) {
        return Err(
            "Адрес Ollama должен быть локальным (http://127.0.0.1:11434): наружу данные не отправляются".into(),
        );
    }
    if cfg.model.trim().is_empty() {
        return Err("Укажите модель Ollama в настройках (например qwen2.5:7b)".into());
    }
    let mut body = json!({
        "model": cfg.model.trim(),
        "stream": false,
        "options": {"temperature": 0.2},
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    if json_mode {
        body["format"] = json!("json");
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(&format!("{}/api/chat", cfg.url.trim().trim_end_matches('/')))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("Ollama недоступна ({e}). Запущена ли она?"))?;
    let text = resp
        .body_mut()
        .with_config()
        .limit(MAX_ANSWER)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|_| format!("непонятный ответ Ollama: {text:.200}"))?;
    if let Some(e) = v["error"].as_str() {
        return Err(format!("Ollama: {e}"));
    }
    v["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "модель вернула пустой ответ".into())
}

const SYS_FOLDER: &str = "Ты помогаешь пользователю Windows понять, что за папка занимает место на диске. \
Тебе дают путь, размер, типы файлов, крупнейшее содержимое и даты — содержимое файлов недоступно. \
Ответь по-русски, коротко (до 6 строк): 1) что это за папка и чья; 2) можно ли удалять и что случится \
(пересоздастся само / пропадут настройки или сохранения / сломается программа); 3) насколько ты уверен. \
Если по этим данным понять нельзя — прямо скажи об этом, не выдумывай. Не советуй удалять системные папки Windows.";

const SYS_SEARCH: &str = "Переведи просьбу пользователя о поиске файлов в JSON-фильтр. Верни ТОЛЬКО JSON-объект с любыми \
из ключей: min_size_mb, max_size_mb (числа, мегабайты), older_than_days, newer_than_days (числа, дни; «старше полугода» = 180, \
«год» = 365), extensions (массив без точки, например [\"zip\",\"rar\",\"7z\",\"exe\",\"msi\"]; «архивы» = zip,rar,7z,tar,gz; \
«установщики» = exe,msi), name_contains (массив фрагментов имени), path_contains (массив фрагментов пути), \
exclude_path_contains (массив фрагментов пути, которые исключить), kind (\"files\" или \"dirs\"), limit (число). \
Не добавляй ключей, которых пользователь не просил.";

const SYS_GROWTH: &str = "Ты помогаешь понять, почему диск Windows внезапно стал занят. Тебе дают папки, чей размер изменился \
между двумя снимками. Ответь по-русски, коротко (до 8 строк): назови вероятных виновников и что с ними делать. Полезные факты: \
ext4.vhdx / *.vhdx в AppData\\Local\\Packages или Docker — образ WSL2/Docker, он сам не сжимается (Optimize-VHD или \
wsl --manage <имя> --set-sparse true); CrashDumps / Minidump / WER — дампы падений; target (Rust), node_modules, .gradle, \
.nuget, pip — кэши сборки; Windows\\Installer, WinSxS, SoftwareDistribution — трогать вручную нельзя, только очистка диска. \
Если по данным причина неочевидна — скажи это, не выдумывай.";

/// The first `{...}` of a model's reply parsed as JSON (models like to wrap it in prose or code fences).
fn json_in(text: &str) -> Option<Value> {
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    serde_json::from_str(text.get(a..=b)?).ok()
}

/// The filter in words, so the user can see how the request was understood.
pub fn describe(f: &Filter) -> String {
    let mut parts = Vec::new();
    parts.push(if f.dirs {
        "папки".to_owned()
    } else {
        "файлы".to_owned()
    });
    if let Some(m) = f.min_size {
        parts.push(format!("от {}", human_size(m)));
    }
    if let Some(m) = f.max_size {
        parts.push(format!("до {}", human_size(m)));
    }
    if let Some(d) = f.older_than_days {
        parts.push(format!("не менялись {d} дн."));
    }
    if let Some(d) = f.newer_than_days {
        parts.push(format!("менялись за {d} дн."));
    }
    for (label, v) in [
        ("расширения", &f.extensions),
        ("имя содержит", &f.name_contains),
        ("путь содержит", &f.path_contains),
        ("кроме пути", &f.exclude_path_contains),
    ] {
        if !v.is_empty() {
            parts.push(format!("{label}: {}", v.join(", ")));
        }
    }
    parts.join(" · ")
}

enum Reply {
    Text { title: String, text: String },
    Search { query: String, filter: Filter },
    Failed(String),
}

#[derive(Default)]
pub struct AiState {
    rx: Option<Receiver<Reply>>,
    /// What is being asked right now (for the spinner).
    busy: Option<String>,
    /// The answer window: `(title, text)`.
    answer: Option<(String, String)>,
}

impl AiState {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
}

impl App {
    fn ai_config(&self) -> Config {
        Config {
            claude: self.settings.ai_claude,
            url: if self.settings.ai_url.trim().is_empty() {
                DEFAULT_URL.into()
            } else {
                self.settings.ai_url.clone()
            },
            model: self.settings.ai_model.clone(),
        }
    }

    fn ai_spawn(&mut self, what: &str, job: impl FnOnce(Config) -> Reply + Send + 'static) {
        if !self.settings.ai_enabled {
            return;
        }
        if self.ai.busy() {
            self.toast("ИИ ещё отвечает на предыдущий вопрос", ToastKind::Info);
            return;
        }
        let cfg = self.ai_config();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(job(cfg));
        });
        self.ai.rx = Some(rx);
        self.ai.busy = Some(what.to_owned());
    }

    /// "What is this folder?" for the node (a folder).
    pub fn ai_explain_folder(&mut self, id: NodeId) {
        let Some(data) = self.data.clone() else { return };
        let tree = &data.out.tree;
        if tree.is_removed(id) || !tree.is_dir(id) {
            return;
        }
        let profile = query::folder_profile(tree, id);
        let title = format!("Что это за папка: {}", tree.name(id));
        self.ai_spawn(&title.clone(), move |cfg| {
            match chat(&cfg, SYS_FOLDER, &profile, false) {
                Ok(text) => Reply::Text { title, text },
                Err(e) => Reply::Failed(e),
            }
        });
    }

    /// Search in plain words: the model only writes a filter, the matching is done here.
    pub fn ai_search(&mut self, request: String) {
        let request = request.trim().to_owned();
        if request.is_empty() || self.data.is_none() {
            return;
        }
        let q = request.clone();
        self.ai_spawn("Ищу…", move |cfg| {
            let reply = chat(&cfg, SYS_SEARCH, &q, true)
                .and_then(|t| json_in(&t).ok_or_else(|| format!("модель не вернула фильтр: {t:.200}")));
            match reply {
                Ok(v) => Reply::Search {
                    query: q,
                    filter: Filter::from_json(&v),
                },
                Err(e) => Reply::Failed(e),
            }
        });
    }

    /// A short explanation of the "what grew" list.
    pub fn ai_explain_growth(&mut self) {
        let Some(data) = self.data.clone() else { return };
        let g = &self.growth;
        if g.changes.is_empty() {
            return;
        }
        let signed = |d: i64| format!("{}{}", if d < 0 { "-" } else { "+" }, human_size(d.unsigned_abs()));
        let mut prompt = format!(
            "Место: {}\nОбщее изменение: {}\nПапки (изменение, путь):\n",
            data.out.tree.path(spacetree_core::ROOT),
            signed(g.total_delta)
        );
        for c in g.changes.iter().take(GROWTH_ROWS) {
            prompt.push_str(&format!("{}  {}\n", signed(c.delta), c.path));
        }
        self.ai_spawn("Разбираю рост…", move |cfg| {
            match chat(&cfg, SYS_GROWTH, &prompt, false) {
                Ok(text) => Reply::Text {
                    title: "Что выросло: разбор".into(),
                    text,
                },
                Err(e) => Reply::Failed(e),
            }
        });
    }

    /// Takes a finished answer, if any. Called every frame.
    pub fn poll_ai(&mut self, ctx: &egui::Context) {
        set_enabled(self.settings.ai_enabled);
        let Some(rx) = &self.ai.rx else { return };
        let reply = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(200));
                return;
            }
            Err(TryRecvError::Disconnected) => Reply::Failed("ответ ИИ прервался".into()),
        };
        self.ai.rx = None;
        self.ai.busy = None;
        match reply {
            Reply::Text { title, text } => self.ai.answer = Some((title, text)),
            Reply::Failed(e) => {
                crate::diag::warn(format!("ai: {e}"));
                self.toast(e, ToastKind::Error);
            }
            Reply::Search { query, filter } => {
                let Some(data) = self.data.clone() else { return };
                let tree = &data.out.tree;
                let hits = query::search(tree, &filter, now_unix());
                let shown = format!("ИИ: {query}");
                self.search = shown.clone();
                self.set_search_hits(&shown, hits.clone());
                let text = format!("Как понят запрос: {}\nНайдено: {}", describe(&filter), hits.len());
                self.ai.answer = Some(("Поиск словами".into(), text));
            }
        }
    }

    /// The floating window with the model's answer, and a hint while it is thinking.
    pub fn ai_window(&mut self, ctx: &egui::Context) {
        if let Some(what) = self.ai.busy.clone() {
            egui::Area::new(egui::Id::new("ai_busy"))
                .anchor(Align2::RIGHT_BOTTOM, vec2(-16.0, -16.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("ИИ думает: {what}"));
                        });
                    });
                });
        }
        let Some((title, text)) = self.ai.answer.clone() else {
            return;
        };
        let mut open = true;
        let mut copy = false;
        egui::Window::new(title)
            .id(egui::Id::new("ai_answer"))
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    ui.label(&text);
                });
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Это подсказка модели, а не гарантия: перед удалением проверьте сами.")
                        .weak()
                        .small(),
                );
                if ui.button("Копировать").clicked() {
                    copy = true;
                }
            });
        if copy {
            ctx.copy_text(text);
        }
        if !open {
            self.ai.answer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_http_addresses_are_accepted() {
        for ok in [
            "http://127.0.0.1:11434",
            "http://localhost:11434/",
            "http://LOCALHOST",
            "http://[::1]:11434",
            "http://127.1.2.3:80",
        ] {
            assert!(is_local_url(ok), "{ok}");
        }
        for bad in [
            "https://127.0.0.1:11434",
            "http://example.com",
            "http://192.168.1.5:11434",
            "http://127.0.0.1@evil.com",
            "http://localhost.evil.com",
            "http://evil.com/127.0.0.1",
            "http://0.0.0.0:11434",
            "",
            "127.0.0.1",
        ] {
            assert!(!is_local_url(bad), "{bad}");
        }
    }

    #[test]
    fn a_filter_is_found_inside_chatty_output() {
        let v = json_in("Конечно!\n```json\n{\"min_size_mb\": 500, \"extensions\": [\"zip\"]}\n```\nГотово").unwrap();
        let f = Filter::from_json(&v);
        assert_eq!(f.min_size, Some(500 << 20));
        assert_eq!(f.extensions, ["zip"]);
        assert!(json_in("нет json").is_none());
        assert!(json_in("{broken").is_none());
        assert!(describe(&f).contains("расширения: zip"));
    }

    #[test]
    fn a_remote_address_is_refused_before_any_request() {
        let cfg = Config {
            claude: false,
            url: "http://example.com".into(),
            model: "m".into(),
        };
        assert!(chat(&cfg, "s", "u", false).unwrap_err().contains("локальным"));
    }
}

#[cfg(test)]
mod claude_live {
    /// Needs a logged-in Claude Code: `cargo test -p spacetree-app claude_live -- --ignored`.
    #[test]
    #[ignore]
    fn claude_answers_without_tools() {
        let cfg = super::Config {
            claude: true,
            url: String::new(),
            model: String::new(),
        };
        let r = super::chat(&cfg, "Отвечай одним словом.", "Столица Франции?", false);
        println!("{r:?}");
        assert!(r.unwrap().to_lowercase().contains("париж"));
    }
}
