//! provfs CLI — mount a provenance-stamping overlay over a directory.

use std::path::PathBuf;

use clap::Parser;
use provfs::{ProvFs, SkipList};

#[derive(Parser, Debug)]
#[command(name = "provfs", version, about = "FUSE-overlay that stamps user.prov.* xattrs at write-time")]
struct Cli {
    /// Backing directory whose writes will be stamped.
    #[arg(long)]
    source: PathBuf,
    /// Mountpoint to expose the overlay at.
    #[arg(long)]
    mount: PathBuf,
    /// Extra skip prefixes (comma-separated), layered on top of defaults.
    #[arg(long, default_value = "")]
    skip: String,
    /// Run mounted in foreground.
    #[arg(long, default_value_t = true)]
    foreground: bool,
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    if !cli.source.is_dir() {
        eprintln!("provfs: --source {:?} is not a directory", cli.source);
        return std::process::ExitCode::from(2);
    }
    if !cli.mount.is_dir() {
        eprintln!("provfs: --mount {:?} is not a directory", cli.mount);
        return std::process::ExitCode::from(2);
    }

    let skip = if cli.skip.is_empty() {
        SkipList::defaults()
    } else {
        SkipList::from_user_spec(&cli.skip)
    };
    let fs = ProvFs::new(cli.source.clone(), skip);

    let opts = vec![
        fuser::MountOption::FSName("provfs".to_string()),
        fuser::MountOption::DefaultPermissions,
    ];

    log::info!(
        "mounting provfs: source={} mount={}",
        cli.source.display(),
        cli.mount.display()
    );
    if let Err(e) = fuser::mount2(fs, &cli.mount, &opts) {
        eprintln!("provfs: mount failed: {e}");
        return std::process::ExitCode::from(3);
    }
    let _ = cli.foreground;
    std::process::ExitCode::SUCCESS
}
