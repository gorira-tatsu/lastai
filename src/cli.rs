use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{
    config::AppConfig,
    index::IndexManager,
    paths::AppPaths,
    providers::{discover_claude_root, discover_codex_root},
    resume::{build_resume_command, run_command},
    scan::{ScanMode, scan_search},
    tui,
    types::{Provider, SearchOptions},
};

#[derive(Debug, Parser)]
#[command(
    name = "lastai",
    version,
    about = "Fast Codex/Claude history search and resume launcher"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Tui(TuiArgs),
    Search(SearchArgs),
    Resume(ResumeArgs),
    Index(IndexArgs),
    Doctor,
}

#[derive(Debug, Args)]
struct TuiArgs {
    #[arg(long, env = "LASTAI_NO_ALT_SCREEN")]
    no_alt_screen: bool,
    #[arg(value_name = "QUERY")]
    query: Vec<String>,
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[arg(value_name = "QUERY")]
    query: Vec<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long)]
    provider: Option<Provider>,
    #[arg(long)]
    all_sidechains: bool,
    #[arg(long, value_enum, default_value_t = SearchBackend::Auto)]
    backend: SearchBackend,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SearchBackend {
    Auto,
    Index,
    Literal,
    Regex,
    Fuzzy,
}

#[derive(Debug, Args)]
struct ResumeArgs {
    session_id: String,
    #[arg(long, value_enum)]
    provider: Provider,
    #[arg(long, default_value = "default")]
    profile: String,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(value_name = "PROMPT")]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
struct IndexArgs {
    #[command(subcommand)]
    command: IndexCommand,
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    Update,
    Rebuild,
    Status,
    Compact,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    let config = AppConfig::load(&paths)?;
    let manager = IndexManager::new(paths.clone(), config.clone());

    match cli.command.unwrap_or(Command::Tui(TuiArgs {
        no_alt_screen: false,
        query: Vec::new(),
    })) {
        Command::Tui(args) => {
            tui::run_tui(manager, config, args.query.join(" "), args.no_alt_screen)
        }
        Command::Search(args) => run_search(manager, args),
        Command::Resume(args) => run_resume(config, args),
        Command::Index(args) => run_index(manager, args.command),
        Command::Doctor => run_doctor(paths, config, manager),
    }
}

fn run_search(manager: IndexManager, args: SearchArgs) -> Result<()> {
    let mut index = manager.load_search_index()?;
    if index.is_empty() && matches!(args.backend, SearchBackend::Index) {
        manager.update()?;
        index = manager.load_search_index()?;
    }
    let mut query = args.query.join(" ");
    if let Some(provider) = args.provider {
        query = format!("provider:{provider} {query}");
    }
    let options = SearchOptions {
        limit: args.limit,
        default_sidechain: (!args.all_sidechains).then_some(false),
        current_cwd: std::env::current_dir().ok(),
    };
    let hits = match args.backend {
        SearchBackend::Index => index.search(&query, options),
        SearchBackend::Literal => scan_search(
            &AppConfig::load(manager.paths())?,
            &query,
            ScanMode::Literal,
            options,
        )?,
        SearchBackend::Regex => scan_search(
            &AppConfig::load(manager.paths())?,
            &query,
            ScanMode::Regex,
            options,
        )?,
        SearchBackend::Fuzzy => scan_search(
            &AppConfig::load(manager.paths())?,
            &query,
            ScanMode::Fuzzy,
            options,
        )?,
        SearchBackend::Auto if index.is_empty() => scan_search(
            &AppConfig::load(manager.paths())?,
            &query,
            ScanMode::Fuzzy,
            options,
        )?,
        SearchBackend::Auto => index.search(&query, options),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        for hit in hits {
            let cwd = hit
                .cwd
                .as_ref()
                .map(|cwd| cwd.display().to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:.2}\t{}\t{}\t{}\t{}",
                hit.score,
                hit.provider,
                hit.session_id,
                cwd,
                hit.timestamp.map(|t| t.to_rfc3339()).unwrap_or_default()
            );
            for snippet in hit.snippets {
                println!("  [{}] {}", snippet.role, snippet.text.replace('\n', " "));
            }
        }
    }
    Ok(())
}

fn run_resume(config: AppConfig, args: ResumeArgs) -> Result<()> {
    let prompt = (!args.prompt.is_empty()).then(|| args.prompt.join(" "));
    let spec = build_resume_command(
        &config,
        args.provider,
        &args.profile,
        &args.session_id,
        prompt.as_deref(),
        args.cwd,
    )?;
    if args.dry_run {
        println!("{}", spec.display());
        return Ok(());
    }
    std::process::exit(run_command(&spec)?);
}

fn run_index(manager: IndexManager, command: IndexCommand) -> Result<()> {
    let stats = match command {
        IndexCommand::Update => manager.update()?,
        IndexCommand::Rebuild => manager.rebuild()?,
        IndexCommand::Status => manager.status()?,
        IndexCommand::Compact => manager.compact()?,
    };
    println!(
        "segments={} docs={} tracked_files={} index_dir={}",
        stats.segments,
        stats.docs,
        stats.tracked_files,
        stats.index_dir.display()
    );
    Ok(())
}

fn run_doctor(paths: AppPaths, config: AppConfig, manager: IndexManager) -> Result<()> {
    let stats = manager.status()?;
    println!("config_file={}", paths.config_file.display());
    println!("cache_dir={}", paths.cache_dir.display());
    println!("index_dir={}", paths.index_dir.display());
    println!("codex_root={}", discover_codex_root().display());
    println!("claude_root={}", discover_claude_root().display());
    println!("codex_command={}", config.providers.codex.command);
    println!("claude_command={}", config.providers.claude.command);
    println!(
        "segments={} docs={} tracked_files={}",
        stats.segments, stats.docs, stats.tracked_files
    );
    Ok(())
}
