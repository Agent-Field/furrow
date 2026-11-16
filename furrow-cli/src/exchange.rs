//! The verbs that move a snapshot between stores as sealed frames. Secrets
//! come from the environment only; no flag carries one.

use clap::Subcommand;
use furrow::exchange::export::DEFAULT_MAX_FRAME;
use furrow::exchange::keys::keys_from_env;
use furrow::exchange::open_store;
use furrow::exchange::ops::{self, ImportMode};
use furrow::exchange::path::{ByteRange, PathError};
use furrow::repository::data_root;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Exchange {
    /// Seal what the named store lacks into frames in an outbox.
    /// Reads FURROW_CELL_KEY and FURROW_DEDUP_SECRET.
    Export {
        /// Snapshot ID to send.
        #[arg(long)]
        head: String,
        #[arg(long)]
        outbox: PathBuf,
        /// Ledger name: 16 lowercase hex characters naming the destination store.
        #[arg(long)]
        ledger: String,
        #[arg(long, default_value_t = DEFAULT_MAX_FRAME)]
        max_frame: usize,
    },
    /// Record uploaded frames in the ledger and delete them.
    Published {
        #[arg(long)]
        ledger: String,
        #[arg(required = true)]
        frames: Vec<PathBuf>,
    },
    /// List the remote ids still needed to complete a snapshot locally.
    /// Reads FURROW_CELL_KEY and FURROW_DEDUP_SECRET.
    ///
    /// With --path, walks only the snapshot, the trees on the path and the
    /// path's own object, and with --range also the chunks covering those
    /// bytes: reading one file (a chat's transcript) must not cost the rest of
    /// the snapshot (its project folder).
    Want {
        #[arg(long)]
        head: String,
        /// Scope the walk to this path of the snapshot, e.g. `.cell/transcript.jsonl`.
        #[arg(long)]
        path: Option<String>,
        /// With --path on a file: also want the chunks overlapping bytes
        /// [S,E). `S:` means to the end of the file.
        #[arg(long, value_name = "S:E", requires = "path")]
        range: Option<String>,
    },
    /// Describe one path of a snapshot from held objects only, never the
    /// network: a file's length and chunk ranges (held or not), or a
    /// directory's entries. A missing index object is `"code":"incomplete"`.
    Peek {
        #[arg(long)]
        head: String,
        #[arg(long)]
        path: String,
    },
    /// Write exactly bytes [S,E) of one file of a snapshot from held chunks
    /// into --out, restoring nothing else. A missing chunk is
    /// `"code":"incomplete"`.
    Read {
        #[arg(long)]
        head: String,
        #[arg(long)]
        path: String,
        #[arg(long, value_name = "S:E")]
        range: String,
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// The verbs this engine has, so a caller can tell a feature is absent
    /// rather than broken.
    Capabilities,
    /// Verify and store the objects in an inbox, one file per remote id.
    /// Reads FURROW_CELL_KEY and FURROW_DEDUP_SECRET.
    Import {
        #[arg(long)]
        head: String,
        #[arg(long)]
        inbox: PathBuf,
        /// Ledger name of the store the objects came from: they are recorded
        /// there as published, so an export never sends them back.
        #[arg(long)]
        ledger: String,
        /// Priming mode: an inbox file no pass wanted is deleted rather than
        /// an error, because the caller knows the inbox holds more than this
        /// head wants.
        #[arg(long)]
        primed: bool,
        /// Streaming mode: store what the walk wants from what has arrived and
        /// leave every other file in place, for an inbox still filling.
        #[arg(long, conflicts_with = "primed")]
        partial: bool,
    },
    /// Tell the engine that the folder it restored into `--from` was moved to
    /// the --repo path, so the tree keeps its history there.
    Rebind {
        #[arg(long)]
        from: PathBuf,
    },
    /// Restore a snapshot held in the store into the --repo folder.
    Materialize {
        #[arg(long)]
        head: String,
        /// Put the sealed `.cell/` entry in this directory instead of the tree.
        #[arg(long, value_name = "DIR")]
        cell_dir: Option<PathBuf>,
    },
}

pub fn run(verb: Exchange, repo: &Path, json: bool) -> anyhow::Result<()> {
    let ok = match execute(verb, repo) {
        Ok(ok) => ok,
        // A path refusal is a JSON object a caller matches by its code, so it
        // is the whole of stderr rather than behind anyhow's `Error: `.
        Err(error) => match error.downcast_ref::<PathError>() {
            Some(refusal) => {
                eprintln!("{refusal}");
                std::process::exit(1);
            }
            None => return Err(error),
        },
    };
    if json {
        println!("{ok}");
    } else {
        println!("{}", describe(&ok));
    }
    Ok(())
}

fn execute(verb: Exchange, repo: &Path) -> anyhow::Result<Value> {
    let data_dir = data_root()?;
    match verb {
        Exchange::Export {
            head,
            outbox,
            ledger,
            max_frame,
        } => ops::export(
            &data_dir,
            &keys_from_env()?,
            &head,
            &ledger,
            &outbox,
            max_frame,
        ),
        Exchange::Published { ledger, frames } => {
            ops::record_published(&data_dir, &ledger, &frames)
        }
        Exchange::Want {
            head,
            path: None,
            range: _,
        } => ops::wanted(&data_dir, &keys_from_env()?, &head),
        Exchange::Want {
            head,
            path: Some(path),
            range,
        } => ops::wanted_path_from_store(
            &open_store(&data_dir)?,
            &keys_from_env()?,
            &head,
            &path,
            range.as_deref().map(ByteRange::parse).transpose()?,
        ),
        Exchange::Peek { head, path } => ops::peek_from_store(
            &open_store(&data_dir)?,
            keys_from_env().ok().as_ref(),
            &head,
            &path,
        ),
        Exchange::Read {
            head,
            path,
            range,
            out,
        } => ops::read_from_store(
            &open_store(&data_dir)?,
            keys_from_env().ok().as_ref(),
            &head,
            &path,
            ByteRange::parse(&range)?,
            &out,
        ),
        Exchange::Capabilities => Ok(ops::capabilities()),
        Exchange::Import {
            head,
            inbox,
            ledger,
            primed,
            partial,
        } => ops::import(
            &data_dir,
            &keys_from_env()?,
            &head,
            &ledger,
            &inbox,
            ImportMode::from_flags(primed, partial)?,
        ),
        Exchange::Rebind { from } => ops::rebind_timeline(
            &data_dir,
            &from,
            repo,
            crate::selected_timeline().as_deref(),
        ),
        Exchange::Materialize { head, cell_dir } => ops::restore_timeline_head(
            &data_dir,
            repo,
            cell_dir,
            &head,
            crate::selected_timeline().as_deref(),
        ),
    }
}

/// One line for a person; scripts use `--json`.
fn describe(ok: &Value) -> String {
    if let Some(frames) = ok["frames"].as_array() {
        return format!(
            "Exported {} objects in {} frames",
            ok["objects"],
            frames.len()
        );
    }
    if let Some(want) = ok["want"].as_array() {
        return format!("{} objects wanted", want.len());
    }
    if let Some(recorded) = ok.get("recorded") {
        return format!("Recorded {recorded} objects");
    }
    if let Some(bytes) = ok.get("bytes") {
        return format!("Wrote {bytes} bytes");
    }
    if ok.get("verbs").is_some() || ok.get("kind").is_some() {
        return ok.to_string();
    }
    if let Some(imported) = ok.get("imported") {
        return format!("Imported {imported} objects");
    }
    if let Some(workspace) = ok["workspace"].as_str() {
        return format!("Workspace {workspace} follows its folder");
    }
    format!(
        "Restored snapshot {}",
        ok["snapshot"].as_str().unwrap_or_default()
    )
}
