use anyhow::{Context, Result};
use clap::Parser;
use indicatif::MultiProgress;
use log::{debug, error, info, warn};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

pub mod filesize_progress_tracker;
mod makemkv;
mod output;

use makemkv::{Drive, MakeMKV};
use output::init_logger;

use crate::output::RipProgressTracker;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Output directory for rips
    #[arg(
        short,
        long,
        env = "AUTO_MAKEMKV_OUTPUT_DIR",
        default_value = "./output"
    )]
    output_dir: PathBuf,

    /// MakeMKV API key (optional if running on a machine with makemkv already configured)
    #[arg(short, long, env = "MAKEMKV_KEY")]
    makemkv_key: Option<String>,

    /// Minimum title length in seconds (not yet implemented in makemkv.rs but good to have)
    #[arg(long, env = "AUTO_MAKEMKV_MIN_LENGTH", default_value = "120")]
    min_length: u64,

    /// Polling interval in seconds
    #[arg(long, env = "AUTO_MAKEMKV_POLL_INTERVAL", default_value = "5")]
    poll_interval: u64,

    /// Whether to run with pretty progress bars (default: true if stdout is a terminal)
    #[arg(long, short, action = clap::ArgAction::Set, default_value = if std::io::stdout().is_terminal() { "true" } else { "false" })]
    interactive: bool,
}

// Shared state removed, using RwLock<()> for synchronization
// Read lock = active rip
// Write lock = reconfiguration

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
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
    let makemkv = Arc::new(MakeMKV::new(args.makemkv_key.as_deref(), &args.output_dir));
    makemkv.init().context("Failed to initialize MakeMKV")?;
    // Use RwLock to manage concurrency between rips and reconfiguration
    let state = Arc::new(RwLock::new(()));

    // Signal handler
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    // Shutdown channel
    let (shutdown_tx, _) = broadcast::channel(1);

    // Initial discovery and task spawning
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // Run initial discovery
    discover_and_spawn(
        &makemkv,
        multi_progress.as_ref(),
        &state,
        &args,
        &mut tasks,
        shutdown_tx.clone(),
    )
    .await?;

    loop {
        tokio::select! {
            _ = sigusr1.recv() => {
                info!("Received SIGUSR1. Attempting reconfiguration...");
                handle_sigusr1(&makemkv, multi_progress.as_ref(), &state, &args, &mut tasks, shutdown_tx.clone()).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C. Shutting down...");
                // Signal shutdown to all tasks
                let _ = shutdown_tx.send(());
                // We can just break and let main exit, which drops tasks.
                break;
            }
        }
    }

    // Wait for tasks to complete
    for task in tasks {
        let _ = task.await;
    }
    info!("Shutdown complete.");

    Ok(())
}

async fn discover_and_spawn(
    makemkv: &Arc<MakeMKV>,
    multi_progress: Option<&MultiProgress>,
    state: &Arc<RwLock<()>>,
    args: &Args,
    tasks: &mut Vec<JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    info!("Discovering drives...");
    let drives = makemkv.discover_drives().await?;

    if drives.is_empty() {
        warn!("No drives found.");
    } else {
        info!("Found {} drives.", drives.len());
    }

    for drive in drives {
        let makemkv = makemkv.clone();
        let multi_progress = multi_progress.cloned();
        let state = state.clone();
        let args = args.clone();
        let shutdown_rx = shutdown_tx.subscribe();

        let task = tokio::spawn(async move {
            drive_task_loop(drive, makemkv, multi_progress, state, args, shutdown_rx).await;
        });
        tasks.push(task);
    }

    Ok(())
}

async fn handle_sigusr1(
    makemkv: &Arc<MakeMKV>,
    multi_progress: Option<&MultiProgress>,
    state: &Arc<RwLock<()>>,
    args: &Args,
    tasks: &mut Vec<JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    // 1) Try to acquire write lock to ensure no rips are active
    // try_write() fails if there are any readers (active rips) or writers
    let lock = match state.try_write() {
        Ok(guard) => guard,
        Err(_) => {
            warn!("Cannot reconfigure: Rips active. Ignoring SIGUSR1.");
            return Ok(());
        }
    };

    // 2) Stop existing drive tasks
    info!("Stopping existing drive tasks...");
    for task in tasks.drain(..) {
        task.abort();
    }

    // 3) Drop lock before spawning new tasks
    // We don't need to hold it while discovering/spawning,
    // and we definitely don't want to hold it if spawn takes time.
    // The old tasks are gone, so no rips are happening.
    drop(lock);

    // 4) Spawn new rip threads
    discover_and_spawn(makemkv, multi_progress, state, args, tasks, shutdown_tx).await?;

    info!("Reconfiguration complete.");
    Ok(())
}

async fn drive_task_loop(
    drive: Drive,
    makemkv: Arc<MakeMKV>,
    multi_progress: Option<MultiProgress>,
    state: Arc<RwLock<()>>,
    args: Args,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let drive_index = drive.index;
    info!(
        "Drive task started for drive {} ({}) - State: {:?}, Disc: {}",
        drive_index, drive.drive_name, drive.state, drive.disc_name
    );

    let mut last_disc_signature: Option<String> = None;

    loop {
        // 1. Sleep (rate limit)
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("Drive {}: Shutdown signal received.", drive_index);
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(args.poll_interval)) => {}
        }

        // 2. Poll for disc info
        info!("Drive {}: Polling for disc info...", drive_index);
        let disc_info_res = tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("Drive {}: Shutdown signal received during poll.", drive_index);
                break;
            }
            res = makemkv.get_disc_info(drive_index) => res
        };

        match disc_info_res {
            Ok((disc_info, _titles)) => {
                if disc_info.disc_type.is_some() {
                    let current_signature = disc_info.get_signature();

                    // Check if this is a new disc
                    let is_new_disc = match &last_disc_signature {
                        None => true,
                        Some(last_sig) => *last_sig != current_signature,
                    };

                    if is_new_disc {
                        let base_name = disc_info.get_base_name();
                        info!("Drive {}: New disc detected: {}", drive_index, base_name);

                        // Acquire "Rip Permit" (Read Lock)
                        // This will wait if a reconfiguration (Write Lock) is active or pending
                        let permit = state.read().await;

                        // Check shutdown again after acquiring lock, as we might have waited
                        if shutdown_rx.try_recv().is_ok() {
                            info!(
                                "Drive {}: Shutdown signal received after acquiring permit.",
                                drive_index
                            );
                            drop(permit);
                            break;
                        }

                        // Rip
                        let rip_result = tokio::select! {
                            _ = shutdown_rx.recv() => {
                                info!("Drive {}: Shutdown signal received during rip.", drive_index);
                                // Permit dropped at end of scope or break
                                break;
                            }
                            res = run_rip_workflow(&makemkv, multi_progress.as_ref(), drive_index, &base_name, &args.output_dir, args.min_length) => res
                        };

                        // Release "Rip Permit"
                        drop(permit);

                        if let Err(e) = rip_result {
                            error!("Drive {}: Rip failed: {:?}", drive_index, e);
                            // Update signature so we don't retry same disc immediately.
                            last_disc_signature = Some(current_signature);
                        } else {
                            info!("Drive {}: Rip complete.", drive_index);
                            last_disc_signature = Some(current_signature);
                        }

                        // Eject
                        info!("Drive {}: Ejecting...", drive_index);
                        // We don't necessarily need to abort eject on shutdown, but we can.
                        let eject_res = tokio::select! {
                             _ = shutdown_rx.recv() => {
                                 info!("Drive {}: Shutdown signal received during eject.", drive_index);
                                 // Try to eject anyway? Or just break.
                                 break;
                             }
                             res = makemkv.eject_disc(&drive) => res
                        };

                        if let Err(e) = eject_res {
                            error!("Drive {}: Eject failed: {:?}", drive_index, e);
                        }

                        // We do NOT wait for removal here explicitly.
                        // The loop will continue, sleep, poll again.
                        // If the disc is still there (eject failed or slow), get_disc_info will return same signature.
                        // is_new_disc will be false.
                        // We will just loop and sleep until signature changes (disc removed -> None, or new disc).
                    } else {
                        // Same disc, do nothing
                        info!(
                            "Drive {}: Disc present but already processed ({}).",
                            drive_index, current_signature
                        );
                    }
                } else {
                    // No disc
                    if last_disc_signature.is_some() {
                        info!("Drive {}: Disc removed.", drive_index);
                        last_disc_signature = None;
                    }
                    info!("Drive {}: Empty.", drive_index);
                }
            }
            Err(e) => {
                warn!(
                    "Drive {}: Error checking disc info: {:?}. Sleeping.",
                    drive_index, e
                );
            }
        }
    }
}

async fn run_rip_workflow(
    makemkv: &MakeMKV,
    multi_progress: Option<&MultiProgress>,
    drive_index: usize,
    base_name: &str,
    output_root: &PathBuf,
    min_length: u64,
) -> Result<()> {
    // Determine output folder name with increment
    let final_output_dir = get_incremented_dir(output_root, base_name)?;

    std::fs::create_dir_all(&final_output_dir)?;
    info!(
        "Drive {}: Ripping {} to {:?}",
        drive_index, base_name, final_output_dir
    );

    // Create progress tracker
    let tracker =
        RipProgressTracker::new(multi_progress, drive_index, base_name, &final_output_dir);
    let tracker_clone = tracker.clone();

    makemkv
        .rip_disc(
            drive_index,
            &final_output_dir,
            min_length,
            move |update| match update {
                makemkv::ProgressUpdate::Progress(p) => {
                    tracker_clone.update_progress(&p);
                }
                makemkv::ProgressUpdate::Message(msg) => {
                    debug!("Drive {}: MakeMKV: {}", drive_index, msg);
                }
                makemkv::ProgressUpdate::ProgressTitle(progress_title) => {
                    tracker_clone.update_title(&progress_title);
                }
            },
        )
        .await?;

    tracker.finish();
    Ok(())
}

fn get_incremented_dir(root: &PathBuf, base_name: &str) -> Result<PathBuf> {
    // Pattern: NN_BaseName
    // Find max NN
    let mut max_n = 0;

    if root.exists() {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Check if name matches NN_BaseName exactly
                    if let Some((num_str, rest)) = name.split_once('_') {
                        if rest == base_name {
                            if let Ok(num) = num_str.parse::<u32>() {
                                if num > max_n {
                                    max_n = num;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let next_n = max_n + 1;
    let folder_name = format!("{:02}_{}", next_n, base_name);
    Ok(root.join(folder_name))
}
