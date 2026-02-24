use anyhow::{Context, Result};
use clap::Parser;
use indicatif::MultiProgress;
use log::info;
use std::sync::Arc;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, RwLock};

mod cli;
mod drive;
pub mod file;
pub mod filesize_progress_tracker;
mod makemkv;
mod notifications;
mod output;
mod transfer;
mod util;

use cli::Args;
use output::init_logger;

struct LockState {
    reconfiguration_active: Mutex<()>,
    rips_active: RwLock<()>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let (Some(token), Some(key)) = (
        args.pushover_app_token.as_deref(),
        args.pushover_user_key.as_deref(),
    ) {
        notifications::init_pushover(token, key);
    }

    // Initialize MultiProgress and Logger
    let multi_progress = if args.interactive {
        Some(MultiProgress::new())
    } else {
        None
    };
    init_logger(multi_progress.clone())?;

    if !args.output_dir.exists() {
        std::fs::create_dir_all(&args.output_dir).context("Failed to create output directory")?;
    }

    info!("Starting auto-makemkv monitoring...");
    info!("Output directory: {:?}", args.output_dir);

    // Core shared components
    let makemkv = Arc::new(makemkv::MakeMKV::new(
        args.makemkv_key.as_deref(),
        args.settings_dir.as_ref(),
    ));
    makemkv.init().context("Failed to initialize MakeMKV")?;
    // Use RwLock to manage concurrency between rips and reconfiguration
    // Use mutex to prevent multiple reconfigurations from being queued.
    let state = Arc::new(LockState {
        reconfiguration_active: Mutex::new(()),
        rips_active: RwLock::new(()),
    });

    // Signal handler
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    let mut cancellation_token = tokio_util::sync::CancellationToken::new();
    drive::discover_and_spawn(
        &makemkv,
        multi_progress.as_ref(),
        &state,
        &args,
        &mut cancellation_token,
    )
    .await?;

    loop {
        tokio::select! {
            _ = sigusr1.recv() => {
                info!("Received SIGUSR1. Attempting reconfiguration...");
                cancellation_token = drive::handle_sigusr1(&makemkv, multi_progress.as_ref(), &state, &args, cancellation_token.clone()).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C. Shutting down...");
                // Signal shutdown to all tasks
                cancellation_token.cancel();
                break;
            }
        }
    }
    Ok(())
}
