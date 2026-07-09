use agit::model::{id_hex, SnapshotTrigger};
use agit::AgitRepository;
use anyhow::Context;
use clap::{Parser, Subcommand};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agit", version, about = "Undo everything between Git commits")]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Attach a repository and create its first complete snapshot.
    Watch,
    /// Create a complete labeled snapshot now.
    Snap {
        #[arg(short, long)]
        message: Option<String>,
    },
    /// List recent workspace snapshots.
    Timeline {
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Preview or restore a previous workspace snapshot.
    Rewind {
        snapshot: String,
        #[arg(long = "paths")]
        paths: Vec<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Show workspace protection and store status.
    Status,
    /// Stop watching this repository.
    Forget {
        #[arg(long)]
        purge: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Watch => {
            let (repository, id) = AgitRepository::watch(&cli.repo)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workspace": repository.root(),
                        "snapshot": id_hex(&id),
                        "store": repository.store_root(),
                    })
                );
            } else {
                println!("Protected {}", repository.root().display());
                println!("Snapshot {}", id_hex(&id));
                println!("Store {}", repository.store_root().display());
            }
        }
        Command::Snap { message } => {
            let mut repository = AgitRepository::open(&cli.repo)?;
            let id = repository.snapshot(message, SnapshotTrigger::Manual)?;
            if cli.json {
                println!("{}", serde_json::json!({"snapshot": id_hex(&id)}));
            } else {
                println!("Snapshot {}", id_hex(&id));
            }
        }
        Command::Timeline { limit } => {
            let repository = AgitRepository::open(&cli.repo)?;
            let timeline = repository.timeline(limit)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&timeline)?);
            } else {
                for snapshot in timeline {
                    println!(
                        "{}  {}  {:<10} {}",
                        &snapshot.id[..12],
                        snapshot.sealed_at,
                        snapshot.trigger,
                        snapshot.label.unwrap_or_default()
                    );
                }
            }
        }
        Command::Rewind {
            snapshot,
            paths,
            dry_run,
            yes,
        } => {
            let mut repository = AgitRepository::open(&cli.repo)?;
            let target = repository.resolve_snapshot(&snapshot)?;
            let plan = repository.plan_rewind(&target, &paths)?;
            if cli.json || dry_run {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print_plan(&plan);
            }
            if dry_run || plan.changes.is_empty() {
                return Ok(());
            }
            if !yes {
                anyhow::ensure!(
                    io::stdin().is_terminal(),
                    "non-interactive rewind requires --yes"
                );
                print!("Apply this rewind? [y/N] ");
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                anyhow::ensure!(answer.trim().eq_ignore_ascii_case("y"), "rewind cancelled");
            }
            let (pre, applied) = repository
                .rewind(&target, &paths)
                .context("rewind failed")?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "restored": applied.target,
                        "pre_rewind_snapshot": id_hex(&pre),
                        "changes": applied.changes.len(),
                    })
                );
            } else {
                println!("Restored {} paths", applied.changes.len());
                println!("Undo snapshot: {}", id_hex(&pre));
            }
        }
        Command::Status => {
            let repository = AgitRepository::open(&cli.repo)?;
            let timeline = repository.timeline(1)?;
            let value = serde_json::json!({
                "workspace": repository.root(),
                "store": repository.store_root(),
                "head": timeline.first().map(|item| item.id.as_str()),
                "protected": !timeline.is_empty(),
            });
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Workspace: {}", repository.root().display());
                println!("Store:     {}", repository.store_root().display());
                if let Some(head) = timeline.first() {
                    println!("Head:      {}", head.id);
                }
            }
        }
        Command::Forget { purge } => {
            AgitRepository::open(&cli.repo)?.forget(purge)?;
            println!("Repository detached from agit");
        }
    }
    Ok(())
}

fn print_plan(plan: &agit::RewindPlan) {
    println!("Rewind to {}", &plan.target[..12]);
    for change in &plan.changes {
        println!("  {:<8} {}", change.action, change.path);
    }
}
