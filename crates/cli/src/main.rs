use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::process::ExitCode;

mod mcp;

use clap::{Parser, Subcommand, ValueEnum};
use spacetree_core::{Backend, Progress, export, scan, stats, stats::human_size as human};

#[derive(Parser)]
#[command(name = "spacetree", about = "Анализатор занятого места на диске")]
struct Cli {
    /// Работать как MCP-сервер (stdin/stdout) для ИИ-агентов: только чтение, без сети
    #[arg(long)]
    mcp: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Просканировать папку или диск
    Scan {
        /// Папка или диск (например C:\ или /home)
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
        backend: BackendArg,
        /// Сколько крупнейших файлов вывести
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Сохранить все узлы в CSV
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Показать, что, вероятно, можно удалить
        #[arg(long)]
        cleanup: bool,
        /// Найти дубликаты по содержимому (размер → хеш начала → полный хеш)
        #[arg(long)]
        dupes: bool,
        /// Показать время по фазам сканирования
        #[arg(long)]
        timings: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendArg {
    Auto,
    Walk,
    Mft,
}

impl From<BackendArg> for Backend {
    fn from(b: BackendArg) -> Self {
        match b {
            BackendArg::Auto => Backend::Auto,
            BackendArg::Walk => Backend::Walk,
            BackendArg::Mft => Backend::Mft,
        }
    }
}

/// `C:` means "current dir on C:" in Windows; the user almost surely means the root.
fn normalize(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if cfg!(windows) && s.len() == 2 && s.ends_with(':') {
        PathBuf::from(format!("{s}\\"))
    } else {
        p
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.mcp {
        mcp::run();
        return ExitCode::SUCCESS;
    }
    let Some(Cmd::Scan {
        path,
        backend,
        top,
        csv,
        cleanup,
        dupes,
        timings,
    }) = cli.cmd
    else {
        eprintln!("Укажите команду (scan) или --mcp; справка: --help");
        return ExitCode::FAILURE;
    };
    let path = normalize(path);
    let progress = Progress::default();
    let out = match scan(&path, backend.into(), &progress) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Ошибка: {e}");
            return ExitCode::FAILURE;
        }
    };

    let tree = &out.tree;
    let (size, alloc, files) = tree.root_stats();
    println!(
        "Бэкенд:           {} ({})",
        out.selection.used.as_str(),
        out.selection.reason
    );
    println!("Файлов:           {files}");
    println!("Папок:            {}", tree.dir_count().saturating_sub(1));
    println!("Размер:           {} ({size} Б)", human(size));
    println!("Размер на диске:  {} ({alloc} Б)", human(alloc));
    println!("Время скана:      {:.3} с", out.elapsed.as_secs_f64());
    println!("Ошибок доступа:   {}", out.errors);
    if out.extra_links > 0 {
        println!("Жёстких ссылок (не учтено повторно): {}", out.extra_links);
    }
    if timings {
        for (name, d) in &out.timings {
            println!("  {:<34}{:>9.3} с", name, d.as_secs_f64());
        }
        for n in &out.notes {
            println!("  {n}");
        }
    }
    for s in out.error_samples.iter().take(5) {
        println!("  ! {s}");
    }

    if top > 0 {
        println!("\nТоп-{top} файлов:");
        for id in stats::top_files(tree, top) {
            println!("{:>12}  {}", human(tree.node(id).size), tree.path(id));
        }
    }

    if cleanup {
        let t = std::time::Instant::now();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        let groups = spacetree_core::cleanup::find(tree, now);
        let total: u64 = groups.iter().map(|g| g.size).sum();
        println!(
            "
Можно освободить (оценка): {} · анализ {:.3} с",
            human(total),
            t.elapsed().as_secs_f64()
        );
        for g in &groups {
            println!(
                "{:>12}  [{}] {} — объектов: {}",
                human(g.size),
                g.category.safety().label(),
                g.category.title(),
                g.items.len()
            );
            for &id in g.items.iter().take(3) {
                println!("{:>12}      {}", human(tree.node(id).size), tree.path(id));
            }
        }
    }

    if dupes {
        let t = std::time::Instant::now();
        let (_, candidates) = spacetree_core::cleanup::find_with_dup_candidates(tree, 0);
        let progress = spacetree_core::dupes::DupProgress::default();
        let sets = spacetree_core::dupes::find(tree, &candidates, &progress).unwrap_or_default();
        let wasted: u64 = sets.iter().map(|s| s.wasted()).sum();
        println!(
            "\nДубликаты по содержимому: наборов {}, лишнего места {} · кандидатов {} · {:.3} с",
            sets.len(),
            human(wasted),
            candidates.len(),
            t.elapsed().as_secs_f64()
        );
        for set in sets.iter().take(10) {
            println!("{:>12}  × {}", human(set.size), set.items.len());
            for &id in &set.items {
                println!("{:>12}      {}", "", tree.path(id));
            }
        }
    }

    if let Some(csv) = csv {
        let res = File::create(&csv).map(BufWriter::new).and_then(|mut w| {
            export::write_csv(tree, &mut w)?;
            std::io::Write::flush(&mut w)
        });
        match res {
            Ok(()) => println!("\nCSV: {}", csv.display()),
            Err(e) => {
                eprintln!("Ошибка записи CSV {}: {e}", csv.display());
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
