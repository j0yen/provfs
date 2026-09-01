//! `prov` — read the `user.prov.*` xattrs that the provfs LSM (or the
//! FUSE overlay in this repo) stamps at write-time.
//!
//! Phase 1 reader per PRD-provenance-fs.md §4.4. Five subcommands:
//! `show`, `who`, `when`, `chain`, `find` — every one takes `--json`
//! for machine-readable output, and none of them treat an unstamped
//! file as an error (PRD §6.1: xattrs are a best-effort hint layer).
//!
//! Deviation from the PRD, noted per the build brief: `chain` here
//! walks `user.prov.history` (the MRU ring on the given file itself),
//! matching PRD §4.4's `prov chain <path> # walk user.prov.history` —
//! not a walk of the path's ancestor directories.

// A reader CLI's entire job is printing to stdout/stderr; the
// print_stdout/print_stderr lints are for library code, not this binary.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use provfs::duration::parse_duration;
use provfs::reader::{ProvRecord, WalkEntry, parse_history, parse_ts, read_record, render_local, walk_files};
use provfs::session::{FallbackFields, SessionKind, classify};

#[derive(Parser, Debug)]
#[command(name = "prov", version, about = "Read user.prov.* provenance xattrs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print every `user.prov.*` key on a path, human-readable.
    Show {
        /// File to inspect.
        path: PathBuf,
        /// Emit machine-readable JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Print just the actor: session id, or comm-chain + uid for a fallback stamp.
    Who {
        /// File to inspect.
        path: PathBuf,
        /// Emit machine-readable JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Print when the file was last stamped.
    When {
        /// File to inspect.
        path: PathBuf,
        /// Emit machine-readable JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Walk `user.prov.history`, the MRU ring of past sessions on this file.
    Chain {
        /// File to inspect.
        path: PathBuf,
        /// Emit machine-readable JSON instead.
        #[arg(long)]
        json: bool,
    },
    /// Recursively find stamped files under a root, filtered by provenance.
    Find {
        /// Root directory to walk.
        root: PathBuf,
        /// Match `user.prov.tool` exactly.
        #[arg(long)]
        tool: Option<String>,
        /// Match files stamped within this long ago (e.g. `24h`, `7d`, `30m`, `10s`).
        #[arg(long)]
        since: Option<String>,
        /// Match `user.prov.session` by prefix.
        #[arg(long)]
        session: Option<String>,
        /// Match `user.prov.intent` exactly.
        #[arg(long)]
        intent: Option<String>,
        /// Emit machine-readable JSON instead.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    sigpipe::reset();
    let cli = Cli::parse();
    match cli.command {
        Command::Show { path, json } => cmd_show(&path, json),
        Command::Who { path, json } => cmd_who(&path, json),
        Command::When { path, json } => cmd_when(&path, json),
        Command::Chain { path, json } => cmd_chain(&path, json),
        Command::Find { root, tool, since, session, intent, json } => {
            cmd_find(&root, tool.as_deref(), since.as_deref(), session.as_deref(), intent.as_deref(), json)
        }
    }
}

/// Render a [`SessionKind`] the way `show`/`who` present the session field.
fn session_kind_human(kind: &SessionKind) -> String {
    match kind {
        SessionKind::AgentNs(id) => format!("agentns session {id}"),
        SessionKind::EnrichedFallback(f) => fallback_human(f),
        SessionKind::LegacyFallback { comm, pid } => format!("fallback: comm={comm} pid={pid}"),
        SessionKind::Opaque(raw) => format!("unrecognized: {raw}"),
    }
}

fn fallback_human(f: &FallbackFields) -> String {
    let chain = f.comm_chain.as_deref().unwrap_or("?");
    let mut s = format!("fallback: comm-chain={chain}");
    if let Some(uid) = &f.uid {
        s.push_str(&format!(" uid={uid}"));
    }
    if let Some(pid) = &f.pid {
        s.push_str(&format!(" pid={pid}"));
    }
    if let Some(cwd) = &f.cwd {
        s.push_str(&format!(" cwd={cwd}"));
    }
    if let Some(env) = &f.env {
        s.push_str(&format!(" env={env}"));
    }
    s
}

fn session_kind_json(kind: &SessionKind) -> Value {
    match kind {
        SessionKind::AgentNs(id) => json!({"kind": "agentns", "id": id}),
        SessionKind::EnrichedFallback(f) => json!({
            "kind": "enriched_fallback",
            "comm_chain": f.comm_chain,
            "env": f.env,
            "cwd": f.cwd,
            "pid": f.pid,
            "uid": f.uid,
        }),
        SessionKind::LegacyFallback { comm, pid } => json!({
            "kind": "legacy_fallback",
            "comm": comm,
            "pid": pid,
        }),
        SessionKind::Opaque(raw) => json!({"kind": "opaque", "raw": raw}),
    }
}

/// `read_record`, reporting a missing/unreadable path as a CLI error
/// (exit 1) distinct from "no provenance" (exit 0).
fn load_record(path: &Path) -> Result<ProvRecord, ExitCode> {
    match read_record(path) {
        Ok(rec) => Ok(rec),
        Err(e) => {
            eprintln!("prov: {}: {e}", path.display());
            Err(ExitCode::FAILURE)
        }
    }
}

fn no_provenance(json: bool) -> ExitCode {
    if json {
        println!("{}", Value::Object(serde_json::Map::new()));
    } else {
        println!("no provenance");
    }
    ExitCode::SUCCESS
}

fn cmd_show(path: &Path, json: bool) -> ExitCode {
    let rec = match load_record(path) {
        Ok(r) => r,
        Err(code) => return code,
    };
    if rec.is_empty() {
        return no_provenance(json);
    }

    if json {
        let session_json = rec.session.as_deref().map(|s| session_kind_json(&classify(s)));
        let ts_json = rec.ts.as_deref().map(|t| {
            let info = parse_ts(t);
            json!({"raw": info.raw, "unix": info.unix, "local": info.unix.and_then(render_local)})
        });
        let out = json!({
            "session": session_json,
            "tool": rec.tool,
            "turn": rec.turn,
            "intent": rec.intent,
            "ts": ts_json,
            "history": rec.history.as_deref().map(parse_history),
        });
        println!("{out}");
        return ExitCode::SUCCESS;
    }

    println!("path:    {}", path.display());
    if let Some(s) = &rec.session {
        println!("session: {} ({})", s, session_kind_human(&classify(s)));
    }
    if let Some(t) = &rec.tool {
        println!("tool:    {t}");
    }
    if let Some(t) = &rec.turn {
        println!("turn:    {t}");
    }
    if let Some(i) = &rec.intent {
        println!("intent:  {i}");
    }
    if let Some(ts) = &rec.ts {
        println!("ts:      {}", ts_human_line(ts));
    }
    if let Some(h) = &rec.history {
        println!("history: {}", parse_history(h).join(", "));
    }
    ExitCode::SUCCESS
}

fn ts_human_line(raw: &str) -> String {
    let info = parse_ts(raw);
    match info.unix.and_then(render_local) {
        Some(local) => format!("{local} (raw: {raw})"),
        None => format!("(unparseable; raw: {raw})"),
    }
}

fn cmd_who(path: &Path, json: bool) -> ExitCode {
    let rec = match load_record(path) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let Some(session) = &rec.session else {
        return no_provenance(json);
    };
    let kind = classify(session);
    if json {
        println!("{}", session_kind_json(&kind));
    } else {
        println!("{}", session_kind_human(&kind));
    }
    ExitCode::SUCCESS
}

fn cmd_when(path: &Path, json: bool) -> ExitCode {
    let rec = match load_record(path) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let Some(ts) = &rec.ts else {
        return no_provenance(json);
    };
    let info = parse_ts(ts);
    if json {
        println!(
            "{}",
            json!({"raw": info.raw, "unix": info.unix, "local": info.unix.and_then(render_local)})
        );
    } else {
        println!("{}", ts_human_line(ts));
    }
    ExitCode::SUCCESS
}

fn cmd_chain(path: &Path, json: bool) -> ExitCode {
    let rec = match load_record(path) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let Some(history) = &rec.history else {
        return no_provenance(json);
    };
    let entries = parse_history(history);
    if entries.is_empty() {
        return no_provenance(json);
    }

    if json {
        let arr: Vec<Value> = entries
            .iter()
            .map(|s| {
                let kind = classify(s);
                let mut v = session_kind_json(&kind);
                if let Value::Object(ref mut m) = v {
                    m.insert("raw".to_string(), Value::String(s.clone()));
                }
                v
            })
            .collect();
        println!("{}", Value::Array(arr));
    } else {
        for (i, s) in entries.iter().enumerate() {
            let kind = classify(s);
            println!("{}: {}", i, session_kind_human(&kind));
        }
    }
    ExitCode::SUCCESS
}

#[allow(clippy::too_many_arguments)]
fn cmd_find(root: &Path, tool: Option<&str>, since: Option<&str>, session: Option<&str>, intent: Option<&str>, json: bool) -> ExitCode {
    let cutoff_unix: Option<i64> = match since {
        None => None,
        Some(s) => match parse_duration(s) {
            Ok(dur) => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                let now_secs = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
                let dur_secs = i64::try_from(dur.as_secs()).unwrap_or(i64::MAX);
                Some(now_secs.saturating_sub(dur_secs))
            }
            Err(e) => {
                eprintln!("prov: invalid --since: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let mut had_error = false;
    let mut matches: Vec<(PathBuf, ProvRecord)> = Vec::new();

    for WalkEntry { path, record } in walk_files(root) {
        let rec = match record {
            Ok(r) => r,
            Err(e) => {
                eprintln!("prov: {}: {e}", path.display());
                had_error = true;
                continue;
            }
        };
        if rec.is_empty() {
            continue;
        }
        if let Some(t) = tool {
            if rec.tool.as_deref() != Some(t) {
                continue;
            }
        }
        if let Some(prefix) = session {
            if !rec.session.as_deref().is_some_and(|s| s.starts_with(prefix)) {
                continue;
            }
        }
        if let Some(i) = intent {
            if rec.intent.as_deref() != Some(i) {
                continue;
            }
        }
        if let Some(cutoff) = cutoff_unix {
            let recent = rec.ts.as_deref().and_then(|t| parse_ts(t).unix).is_some_and(|u| u >= cutoff);
            if !recent {
                continue;
            }
        }
        matches.push((path, rec));
    }

    if json {
        let arr: Vec<Value> = matches
            .iter()
            .map(|(path, rec)| {
                json!({
                    "path": path.display().to_string(),
                    "session": rec.session,
                    "tool": rec.tool,
                    "turn": rec.turn,
                    "intent": rec.intent,
                    "ts": rec.ts,
                })
            })
            .collect();
        println!("{}", Value::Array(arr));
    } else {
        for (path, rec) in &matches {
            println!(
                "{}\ttool={}\tsession={}\tts={}",
                path.display(),
                rec.tool.as_deref().unwrap_or("-"),
                rec.session.as_deref().unwrap_or("-"),
                rec.ts.as_deref().unwrap_or("-"),
            );
        }
    }

    if had_error && matches.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
