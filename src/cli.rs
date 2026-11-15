//! Command line. With no subcommand, paths open the browser explorer.

use crate::engine::{self, Engine, LoadRequest};
use crate::mcp;
use crate::tui;
use crate::web::{self, ServeOpts};
use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "logfog",
    version,
    about = "Cut through the log fog. One index, two faces."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Severity totals, time span, top error templates, anomaly windows.
    Summary(Common),
    /// Drain templates sorted by count or spike.
    Patterns(PatternArgs),
    /// Run a facet / regex / time query.
    Query(QueryArgs),
    /// Chronological events for a trace, request, or correlation id.
    Trace(TraceArgs),
    /// Open the loopback browser explorer.
    Ui(Common),
    /// Full-screen terminal view.
    Tui(Common),
    /// JSON-RPC MCP server on stdio.
    Mcp(Common),
}

#[derive(Args, Debug)]
struct Common {
    /// SSH source, user@host:/absolute/path (repeatable). Uses the local ssh binary.
    #[arg(long)]
    ssh: Vec<String>,
    /// Docker container (repeatable). Uses the local docker binary.
    #[arg(long)]
    docker: Vec<String>,
    /// systemd unit (repeatable). Uses the local journalctl binary.
    #[arg(long)]
    journald: Vec<String>,
    /// Keep tailing and stream new lines (ui, and the default when stdin is a pipe).
    #[arg(long)]
    follow: bool,
    /// Loopback port for the browser explorer.
    #[arg(long, default_value_t = 7474)]
    port: u16,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Filter expression. Also used by summary and patterns.
    #[arg(long, short = 'q', default_value = "")]
    query: String,
    /// SQLite file for bookmarks and saved views.
    #[arg(long)]
    db: Option<PathBuf>,
    /// How many lines to pull from docker logs / journalctl.
    #[arg(long, default_value_t = 5000)]
    remote_lines: usize,
    /// Files, directories, or globs. `-` reads stdin.
    paths: Vec<String>,
}

#[derive(Args, Debug)]
struct PatternArgs {
    #[command(flatten)]
    common: Common,
    /// `count` or `spike`.
    #[arg(long, default_value = "count")]
    sort: String,
    #[arg(long)]
    level: Option<String>,
    #[arg(long)]
    service: Option<String>,
}

#[derive(Args, Debug)]
struct QueryArgs {
    /// Query expression. Example: 'level:error status:>=500'.
    query: String,
    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug)]
struct TraceArgs {
    /// trace_id, request_id, span_id, or correlation_id value.
    id: String,
    #[command(flatten)]
    common: Common,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse_from(normalize_args(std::env::args()));
    match cli.command {
        Command::Summary(common) => cmd_summary(common),
        Command::Patterns(args) => cmd_patterns(args),
        Command::Query(args) => cmd_query(args),
        Command::Trace(args) => cmd_trace(args),
        Command::Ui(common) => cmd_ui(common),
        Command::Tui(common) => cmd_tui(common),
        Command::Mcp(common) => {
            let engine = load_common(&common)?;
            mcp::serve(&engine)
        }
    }
}

fn cmd_summary(common: Common) -> Result<()> {
    let engine = load_common(&common)?;
    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&engine.summary(&common.query)?)?
        );
    } else {
        println!("{}", engine.summary_text(&common.query, 500)?);
    }
    Ok(())
}

fn cmd_patterns(args: PatternArgs) -> Result<()> {
    let engine = load_common(&args.common)?;
    let mut q = args.common.query.clone();
    if let Some(level) = &args.level {
        q.push_str(&format!(" level:{level}"));
    }
    if let Some(service) = &args.service {
        q.push_str(&format!(" service:{service}"));
    }
    let rows = engine.patterns(q.trim(), &args.sort)?;
    if args.common.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!("count\terrors\ttemplate");
        for row in rows.iter().take(40) {
            let errors = row.levels.get("error").copied().unwrap_or(0)
                + row.levels.get("fatal").copied().unwrap_or(0);
            println!("{}\t{errors}\t{}", row.count, row.template);
        }
    }
    Ok(())
}

fn cmd_query(args: QueryArgs) -> Result<()> {
    let engine = load_common(&args.common)?;
    let q = if args.common.query.is_empty() {
        args.query.as_str()
    } else {
        args.common.query.as_str()
    };
    let page = engine.query_page(engine::PageQuery {
        query: q,
        from_ns: None,
        to_ns: None,
        offset: 0,
        limit: 200,
        token_budget: None,
        dedup: false,
    })?;
    if args.common.json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else {
        println!("total={} showing={}", page.total, page.events.len());
        for ev in &page.events {
            let ts = ev.ts.clone().unwrap_or_else(|| "-".into());
            println!("{ts} {:<5} {:<12} {}", ev.level, ev.service, ev.message);
        }
    }
    Ok(())
}

fn cmd_trace(args: TraceArgs) -> Result<()> {
    let engine = load_common(&args.common)?;
    let events = engine.trace(&args.id);
    if args.common.json {
        println!("{}", serde_json::to_string_pretty(&events)?);
    } else if events.is_empty() {
        println!("no events for {}", args.id);
    } else {
        for ev in &events {
            let ts = ev.ts.clone().unwrap_or_else(|| "-".into());
            println!("{ts} {} {} {}", ev.service, ev.level, ev.message);
        }
    }
    Ok(())
}

fn cmd_ui(common: Common) -> Result<()> {
    let follow_stdin = stdin_wanted(&common) && !std::io::stdin().is_terminal();
    let follow_files = if common.follow {
        engine::followed_paths(&common.paths)?
    } else {
        Vec::new()
    };
    // Batch-load files first. A piped stdin is tailed by the server so the UI
    // can open before the producer exits. Followed remote sources are read by
    // the tailer (`tail -F` / `docker logs -f`), not twice.
    let mut req = request_from(&common);
    req.read_stdin = false;
    if common.follow {
        req.ssh.clear();
        req.docker.clear();
        req.journald.clear();
    }
    let engine = if common.paths.is_empty()
        && common.ssh.is_empty()
        && common.docker.is_empty()
        && common.journald.is_empty()
        && !follow_stdin
        && std::io::stdin().is_terminal()
    {
        return Err(anyhow!(
            "pass log files, a glob, --ssh, --docker, --journald, or pipe stdin. Try `logfog --help`."
        ));
    } else {
        engine::load_batch(&req)?
    };
    let opts = ServeOpts {
        port: common.port,
        db_path: common.db.unwrap_or_else(|| PathBuf::from("logfog.sqlite")),
        follow_files,
        follow_stdin,
        ssh: if common.follow {
            common.ssh.clone()
        } else {
            Vec::new()
        },
        docker: if common.follow {
            common.docker.clone()
        } else {
            Vec::new()
        },
        journald: if common.follow {
            common.journald.clone()
        } else {
            Vec::new()
        },
        remote_lines: common.remote_lines,
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(web::serve(engine, opts))
}

fn cmd_tui(common: Common) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(anyhow!("logfog tui needs a terminal"));
    }
    let engine = load_common(&common)?;
    tui::run(&engine)
}

fn load_common(common: &Common) -> Result<Engine> {
    let mut req = request_from(common);
    req.read_stdin = stdin_wanted(common) && !std::io::stdin().is_terminal();
    engine::load_batch(&req)
}

fn request_from(common: &Common) -> LoadRequest {
    LoadRequest {
        paths: common
            .paths
            .iter()
            .filter(|p| p.as_str() != "-")
            .cloned()
            .collect(),
        ssh: common.ssh.clone(),
        docker: common.docker.clone(),
        journald: common.journald.clone(),
        read_stdin: false,
        remote_lines: common.remote_lines,
    }
}

fn stdin_wanted(common: &Common) -> bool {
    common.paths.iter().any(|p| p == "-")
        || (common.paths.is_empty()
            && common.ssh.is_empty()
            && common.docker.is_empty()
            && common.journald.is_empty())
}

fn normalize_args<I>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    if args.len() <= 1 {
        let mut out = args;
        out.push("ui".into());
        return out;
    }
    if args
        .iter()
        .any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V")
        && !args.iter().any(|a| is_command(a))
    {
        return args;
    }
    if args
        .iter()
        .skip(1)
        .any(|a| is_command(a) && !is_flag_value(&args, a))
    {
        return args;
    }
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(args[0].clone());
    out.push("ui".into());
    out.extend(args.into_iter().skip(1));
    out
}

fn is_command(arg: &str) -> bool {
    matches!(
        arg,
        "summary" | "patterns" | "query" | "trace" | "ui" | "tui" | "mcp"
    )
}

fn is_flag_value(args: &[String], arg: &str) -> bool {
    let values = [
        "--ssh",
        "--docker",
        "--journald",
        "--port",
        "--query",
        "-q",
        "--db",
        "--sort",
        "--level",
        "--service",
        "--remote-lines",
    ];
    args.windows(2)
        .any(|w| values.contains(&w[0].as_str()) && w[1] == arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_ui_for_bare_paths() {
        let args = normalize_args(["logfog".into(), "./app.log".into()]);
        assert_eq!(args, vec!["logfog", "ui", "./app.log"]);
    }

    #[test]
    fn keeps_explicit_commands() {
        let args = normalize_args(["logfog".into(), "summary".into(), "a.log".into()]);
        assert_eq!(args[1], "summary");
    }
}
