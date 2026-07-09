use agit::model::{id_hex, SnapshotTrigger};
use agit::AgitRepository;
use anyhow::Context;
use clap::{Parser, Subcommand};
use std::ffi::OsString;
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
    Watch {
        /// Keep running and seal after filesystem write quiescence.
        #[arg(long)]
        foreground: bool,
        /// Attach and snapshot without leaving a background watcher running.
        #[arg(long, conflicts_with = "foreground")]
        no_daemon: bool,
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
    },
    #[command(name = "__daemon", hide = true)]
    Daemon {
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
    },
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
        /// Restore SQLite databases from their logically consistent backup image.
        #[arg(long)]
        sqlite_consistent: bool,
    },
    /// Show workspace protection and store status.
    Status,
    /// Create an isolated full-state workspace, optionally running a command inside it.
    Fork {
        /// Stable name used by `agit forks` and as the default directory name.
        name: Option<String>,
        /// Destination directory. Defaults to <repo>.agit-forks/<name> beside the repository.
        #[arg(long)]
        destination: Option<PathBuf>,
        /// Command to run inside the completed fork, supplied after `--`.
        #[arg(last = true)]
        command: Vec<OsString>,
    },
    /// List full-state workspace forks created from this repository.
    Forks,
    /// Stop watching this repository.
    Forget {
        #[arg(long)]
        purge: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Watch {
            foreground,
            no_daemon,
            debounce_ms,
        } => {
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
            if foreground {
                agit::watcher::run(
                    repository,
                    std::time::Duration::from_millis(debounce_ms.max(10)),
                )?;
            } else if !no_daemon && std::env::var_os("AGIT_NO_DAEMON").is_none() {
                spawn_background_watcher(&repository, debounce_ms.max(10))?;
            }
        }
        Command::Daemon { debounce_ms } => {
            let repository = AgitRepository::open(&cli.repo)?;
            agit::watcher::run(
                repository,
                std::time::Duration::from_millis(debounce_ms.max(10)),
            )?;
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
            sqlite_consistent,
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
                .rewind(&target, &paths, sqlite_consistent)
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
            let status = repository.status()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Workspace: {}", status.workspace.display());
                println!("Store:     {}", status.store.display());
                if let Some(head) = status.head {
                    println!("Head:      {head}");
                }
                println!("Snapshots: {}", status.snapshots);
                println!("Objects:   {}", status.objects);
                println!("Pack data: {} bytes", status.physical_bytes);
                println!(
                    "Watcher:   {}",
                    if status.watcher_running {
                        "running"
                    } else {
                        "stopped"
                    }
                );
            }
        }
        Command::Fork {
            name,
            destination,
            command,
        } => {
            anyhow::ensure!(
                !cli.json || command.is_empty(),
                "--json cannot be combined with a fork command"
            );
            let mut repository = AgitRepository::open(&cli.repo)?;
            let name = name.unwrap_or_else(default_fork_name);
            let destination =
                destination.unwrap_or_else(|| default_fork_destination(&repository, &name));
            let summary = repository.fork(&name, &destination)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!("Fork {}", summary.name);
                println!("Path {}", summary.destination.display());
                println!(
                    "Tier {} | {} files | {} logical bytes | {} cloned | {} copied | {} ms",
                    summary.tier,
                    summary.files,
                    summary.logical_bytes,
                    summary.cloned_bytes,
                    summary.copied_bytes,
                    summary.elapsed_ms
                );
                println!("Base {}", &summary.base_snapshot[..12]);
            }
            if let Some((program, arguments)) = command.split_first() {
                let status = std::process::Command::new(program)
                    .args(arguments)
                    .current_dir(&summary.destination)
                    .env("AGIT_FORK_NAME", &summary.name)
                    .env("AGIT_FORK_BASE", &summary.base_snapshot)
                    .status()
                    .with_context(|| format!("run {:?} in fork", program))?;
                anyhow::ensure!(status.success(), "fork command exited with {status}");
            }
        }
        Command::Forks => {
            let repository = AgitRepository::open(&cli.repo)?;
            let forks = repository.forks()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&forks)?);
            } else if forks.is_empty() {
                println!("No forks");
            } else {
                for fork in forks {
                    println!(
                        "{:<20} {:<14} {:>8} ms  {}",
                        fork.name,
                        fork.tier,
                        fork.elapsed_ms,
                        fork.destination.display()
                    );
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

fn default_fork_name() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("fork-{seconds}")
}

fn default_fork_destination(repository: &AgitRepository, name: &str) -> PathBuf {
    let root = repository.root();
    let parent = root.parent().unwrap_or_else(|| std::path::Path::new("."));
    let repository_name = root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("workspace"));
    let mut forks_name = repository_name.to_os_string();
    forks_name.push(".agit-forks");
    parent.join(forks_name).join(name)
}

fn print_plan(plan: &agit::RewindPlan) {
    println!("Rewind to {}", &plan.target[..12]);
    for change in &plan.changes {
        println!("  {:<8} {}", change.action, change.path);
    }
}

fn spawn_background_watcher(repository: &AgitRepository, debounce_ms: u64) -> anyhow::Result<()> {
    let daemon_dir = repository.workspace_data_dir();
    std::fs::create_dir_all(&daemon_dir)?;
    let pid_path = daemon_dir.join("daemon.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            if unsafe { libc::kill(pid, 0) } == 0 {
                println!("Watcher already running with PID {pid}");
                return Ok(());
            }
        }
    }

    let log_path = daemon_dir.join("daemon.log");
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = stdout.try_clone()?;
    let child = std::process::Command::new(std::env::current_exe()?)
        .arg("--repo")
        .arg(repository.root())
        .arg("__daemon")
        .arg("--debounce-ms")
        .arg(debounce_ms.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;
    std::fs::write(&pid_path, format!("{}\n", child.id()))?;
    println!("Watcher started with PID {}", child.id());
    println!("Log {}", log_path.display());
    Ok(())
}
