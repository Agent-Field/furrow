use clap::Subcommand;
use std::path::PathBuf;
#[derive(Subcommand)]
pub enum Archive {
    Capture {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
    },
    Restore {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        snapshot: String,
        #[arg(long)]
        destination: PathBuf,
    },
    VerifyRemainder {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        snapshot: String,
        #[arg(long)]
        workspace: PathBuf,
    },
    Verify {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        snapshot: String,
        #[arg(long)]
        workspace: PathBuf,
    },
}
pub fn run(command: Archive) -> anyhow::Result<()> {
    let receipt = match command {
        Archive::Capture { store, workspace } => furrow::archive::capture(&store, &workspace)?,
        Archive::Restore {
            store,
            snapshot,
            destination,
        } => furrow::archive::restore(&store, &snapshot, &destination)?,
        Archive::VerifyRemainder {
            store,
            snapshot,
            workspace,
        } => furrow::archive::verify_remainder(&store, &snapshot, &workspace)?,
        Archive::Verify {
            store,
            snapshot,
            workspace,
        } => furrow::archive::verify(&store, &snapshot, &workspace)?,
    };
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(())
}
