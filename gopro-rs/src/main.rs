mod camera;
mod cli;
mod config;
mod player;
mod tui;

use anyhow::Result;
use camera::Source;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Browse and import GoPro media to your Mac — over USB (GoPro Connect), Wi-Fi, or a mounted SD card.
///
/// With no subcommand, opens the full-screen browser / gallery.
#[derive(Parser)]
#[command(name = "gopro", version, about, long_about = None)]
struct Cli {
    /// Camera address (auto: USB 172.2x.1yz.51, else Wi-Fi 10.5.5.9)
    #[arg(long, global = true)]
    ip: Option<String>,
    /// Camera HTTP port
    #[arg(long, global = true, default_value_t = 8080)]
    port: u16,
    /// Read from a mounted SD card / DCIM folder instead of the camera API
    #[arg(long, global = true, value_name = "PATH")]
    sd: Option<PathBuf>,
    /// Import destination (default: last folder chosen in the TUI, else ~/Movies/GoPro)
    #[arg(long, global = true)]
    dest: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Full-screen browser with list view, gallery and import (default)
    #[command(alias = "ui", alias = "gallery")]
    Tui {
        /// Don't create YYYY-MM-DD subfolders
        #[arg(long)]
        flat: bool,
        /// Parallel connections per file when importing
        #[arg(long, default_value_t = 6)]
        streams: usize,
    },
    /// Show how the camera is located and whether it answers
    Find,
    /// Camera model, battery, card space, file counts
    Status,
    /// List media on the camera
    Ls {
        /// Only files shot on/after this date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Include .LRV proxies / .THM thumbnails
        #[arg(long)]
        all: bool,
    },
    /// Import media to the Mac (resumable; skips files already present)
    Pull {
        /// Specific file names (default: everything)
        names: Vec<String>,
        #[arg(long)]
        since: Option<String>,
        /// Show what would be imported, copy nothing
        #[arg(long)]
        dry_run: bool,
        /// Delete each file from the camera after a verified copy
        #[arg(long)]
        delete_after: bool,
        /// Also fetch .LRV / .THM sidecars
        #[arg(long)]
        include_lrv: bool,
        /// Don't create YYYY-MM-DD subfolders
        #[arg(long)]
        flat: bool,
        /// Parallel connections per file (1 = single stream)
        #[arg(long, default_value_t = 6)]
        streams: usize,
    },
    /// Play a video in an external player (VLC / IINA / mpv / QuickTime), streamed from the camera
    Play {
        name: String,
        /// Play the full-quality file instead of the low-res proxy
        #[arg(long)]
        full: bool,
    },
    /// Delete files from the camera
    Rm {
        names: Vec<String>,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Open the import folder in Finder
    Open,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let dest = match &cli.dest {
        Some(d) => config::expand(d),
        None => config::load_dest().unwrap_or_else(config::default_dest),
    };
    let connect = || Source::connect(cli.ip.as_deref(), cli.port, cli.sd.as_deref());
    match cli.cmd.unwrap_or(Cmd::Tui { flat: false, streams: 6 }) {
        Cmd::Find => {
            cli::find(cli.ip.as_deref(), cli.port);
            Ok(())
        }
        Cmd::Open => {
            std::fs::create_dir_all(&dest)?;
            std::process::Command::new("open").arg(&dest).status()?;
            Ok(())
        }
        Cmd::Status => cli::status(&connect()?),
        Cmd::Ls { since, all } => cli::ls(&connect()?, &cli::Filter { since: since.as_deref(), names: &[], include_lrv: all }),
        Cmd::Pull { names, since, dry_run, delete_after, include_lrv, flat, streams } => cli::pull(
            &connect()?,
            &cli::Filter { since: since.as_deref(), names: &names, include_lrv },
            &cli::PullOpts { dest: &dest, flat, dry_run, delete_after, streams },
        ),
        Cmd::Play { name, full } => cli::play(&connect()?, &name, full),
        Cmd::Rm { names, yes } => cli::rm(&connect()?, &names, yes),
        Cmd::Tui { flat, streams } => {
            eprintln!("connecting…");
            let src = connect()?;
            tui::run(src, dest, flat, streams)
        }
    }
}
