use anyhow::{Context, Result};
use clap::Parser;
use indicatif::MultiProgress;
use log::{debug, error, info, warn};
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

use crate::output::{create_current_progress_bar, create_total_progress_bar};

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

    /// Minimum title length in seconds (not yet implemented in makemkv.rs but good to have)
    #[arg(long, env = "AUTO_MAKEMKV_MIN_LENGTH", default_value = "120")]
    min_length: u64,

    /// Polling interval in seconds
    #[arg(long, env = "AUTO_MAKEMKV_POLL_INTERVAL", default_value = "5")]
    poll_interval: u64,
}

// Shared state removed, using RwLock<()> for synchronization
// Read lock = active rip
// Write lock = reconfiguration

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize MultiProgress and Logger
    let multi_progress = MultiProgress::new();
    init_logger(multi_progress.clone())?;

    let args = Args::parse();

    if !args.output_dir.exists() {
        std::fs::create_dir_all(&args.output_dir).context("Failed to create output directory")?;
    }

    info!("Starting auto-makemkv monitoring...");
    info!("Output directory: {:?}", args.output_dir);

    // Core shared components
    let makemkv = Arc::new(MakeMKV::new());
    // output removed, using multi_progress passed directly
    // Use RwLock to manage concurrency between rips and reconfiguration
    let state = Arc::new(RwLock::new(()));

    // Notification removed as we use RwLock fairness

    // Signal handler
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    // Shutdown channel
    let (shutdown_tx, _) = broadcast::channel(1);

    // Initial discovery and task spawning
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // Run initial discovery
    discover_and_spawn(
        &makemkv,
        &multi_progress,
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
                handle_sigusr1(&makemkv, &multi_progress, &state, &args, &mut tasks, shutdown_tx.clone()).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C. Shutting down...");
                // Signal shutdown to all tasks
                let _ = shutdown_tx.send(());
                // Wait for tasks to finish?
                // We can just break and let main exit, which drops tasks.
                // But explicit wait is cleaner if we want to log completion.
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
    multi_progress: &MultiProgress,
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
        let multi_progress = multi_progress.clone();
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
    multi_progress: &MultiProgress,
    state: &Arc<RwLock<()>>,
    args: &Args,
    tasks: &mut Vec<JoinHandle<()>>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    // 1) Try to acquire write lock to ensure no rips are active
    // try_write() fails if there are any readers (active rips) or writers
    let _lock = match state.try_write() {
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
    drop(_lock);

    // 4) Spawn new rip threads
    discover_and_spawn(makemkv, multi_progress, state, args, tasks, shutdown_tx).await?;

    info!("Reconfiguration complete.");
    Ok(())
}

async fn drive_task_loop(
    drive: Drive,
    makemkv: Arc<MakeMKV>,
    multi_progress: MultiProgress,
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
                            res = run_rip_workflow(&makemkv, &multi_progress, drive_index, &base_name, &args.output_dir) => res
                        };

                        // Release "Rip Permit"
                        drop(permit);

                        if let Err(e) = rip_result {
                            error!("Drive {}: Rip failed: {:?}", drive_index, e);
                            // If rip failed, do we update signature?
                            // If we don't, we might retry infinitely.
                            // Better to update signature so we don't retry same disc immediately.
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
    multi_progress: &MultiProgress,
    drive_index: usize,
    base_name: &str,
    output_root: &PathBuf,
) -> Result<()> {
    // Determine output folder name with increment
    let final_output_dir = get_incremented_dir(output_root, base_name)?;

    std::fs::create_dir_all(&final_output_dir)?;
    info!(
        "Drive {}: Ripping {} to {:?}",
        drive_index, base_name, final_output_dir
    );

    // Create progress bar
    let current_bar = create_current_progress_bar(
        multi_progress,
        100,
        &format!("Drive {}: Ripping {}...", drive_index, base_name),
        &final_output_dir,
    );
    let total_bar = create_total_progress_bar(
        multi_progress,
        100,
        &format!("Drive {}: Ripping {}...", drive_index, base_name),
        &current_bar,
    );

    let total_bar_clone = total_bar.clone();
    let current_bar_clone = current_bar.clone();
    makemkv
        .rip_disc(drive_index, &final_output_dir, move |update| match update {
            makemkv::ProgressUpdate::Progress(p) => {
                total_bar_clone.set_length(p.max);
                current_bar_clone.set_length(p.max);
                total_bar_clone.set_position(p.total);
                current_bar_clone.set_position(p.current);
            }
            makemkv::ProgressUpdate::Message(msg) => {
                debug!("Drive {}: MakeMKV: {}", drive_index, msg);
            }
            makemkv::ProgressUpdate::ProgressTitle(progress_title) => {
                match progress_title.title_type {
                    makemkv::ProgressTitleType::Current => {
                        current_bar_clone.set_message(format!(
                            "Drive {}: Title {}: {}",
                            drive_index, progress_title.id, progress_title.name
                        ));
                    }
                    makemkv::ProgressTitleType::Total => {
                        total_bar_clone.set_message(format!(
                            "Drive {}: Title {}: {}",
                            drive_index, progress_title.id, progress_title.name
                        ));
                    }
                }
            }
        })
        .await?;

    current_bar.finish_and_clear();
    total_bar.finish_with_message(format!(
        "Drive {}: Finished ripping {}",
        drive_index, base_name
    ));
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
