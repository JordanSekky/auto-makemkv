use anyhow::Result;
use indicatif::MultiProgress;
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::cli::Args;
use crate::makemkv::{Drive, MakeMKV, ProgressUpdate};
use crate::notifications;
use crate::output::RipProgressTracker;
use crate::transfer::move_rip_dir;
use crate::util::get_incremented_dir;

pub async fn discover_and_spawn(
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

pub async fn handle_sigusr1(
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

                        // Prepare output directory
                        let final_output_dir =
                            match get_incremented_dir(&args.output_dir, &base_name) {
                                Ok(dir) => dir,
                                Err(e) => {
                                    error!(
                                        "Drive {}: Failed to determine output dir: {:?}",
                                        drive_index, e
                                    );
                                    drop(permit);
                                    last_disc_signature = Some(current_signature);
                                    continue;
                                }
                            };
                        if let Err(e) = std::fs::create_dir_all(&final_output_dir) {
                            error!(
                                "Drive {}: Failed to create output dir: {:?}",
                                drive_index, e
                            );
                            drop(permit);
                            last_disc_signature = Some(current_signature);
                            continue;
                        }

                        if let Some(p) = notifications::pushover() {
                            p.notify_rip_started(drive_index, &base_name, &final_output_dir)
                                .await;
                        }

                        // Rip
                        let rip_result = tokio::select! {
                            _ = shutdown_rx.recv() => {
                                info!("Drive {}: Shutdown signal received during rip.", drive_index);
                                break;
                            }
                            res = run_rip_workflow(&makemkv, multi_progress.as_ref(), drive_index, &base_name, &final_output_dir, args.min_length) => res
                        };

                        // Release "Rip Permit"
                        drop(permit);

                        match &rip_result {
                            Ok(()) => {
                                info!("Drive {}: Rip complete.", drive_index);
                                // If only --failed-dir was given (no --completed-dir), leave
                                // successes in place. Otherwise use --completed-dir.
                                move_rip_dir(
                                    drive_index,
                                    &final_output_dir,
                                    args.completed_dir.as_ref(),
                                )
                                .await;
                                if let Some(p) = notifications::pushover() {
                                    p.notify_rip_completed(drive_index, &base_name).await;
                                }
                            }
                            Err(e) => {
                                error!("Drive {}: Rip failed: {:?}", drive_index, e);
                                let is_empty = std::fs::read_dir(&final_output_dir)
                                    .map(|mut d| d.next().is_none())
                                    .unwrap_or(true);
                                if is_empty {
                                    let _ = std::fs::remove_dir(&final_output_dir);
                                } else {
                                    // failed_dir wins; if absent but completed_dir is set, use
                                    // that (move all terminal-state rips there).
                                    let dest_dir =
                                        args.failed_dir.as_ref().or(args.completed_dir.as_ref());
                                    move_rip_dir(drive_index, &final_output_dir, dest_dir).await;
                                }
                                if let Some(p) = notifications::pushover() {
                                    p.notify_rip_failed(drive_index, &base_name).await;
                                }
                            }
                        }
                        last_disc_signature = Some(current_signature);

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
    output_dir: &std::path::PathBuf,
    min_length: u64,
) -> Result<()> {
    info!(
        "Drive {}: Ripping {} to {:?}",
        drive_index, base_name, output_dir
    );

    let tracker = RipProgressTracker::new(multi_progress, drive_index, base_name, output_dir);
    let tracker_clone = tracker.clone();

    makemkv
        .rip_disc(
            drive_index,
            output_dir,
            min_length,
            move |update| match update {
                ProgressUpdate::Progress(p) => {
                    tracker_clone.update_progress(&p);
                }
                ProgressUpdate::Message(msg) => {
                    debug!("Drive {}: MakeMKV: {}", drive_index, msg);
                }
                ProgressUpdate::ProgressTitle(progress_title) => {
                    tracker_clone.update_title(&progress_title);
                }
            },
        )
        .await?;

    tracker.finish();
    Ok(())
}
