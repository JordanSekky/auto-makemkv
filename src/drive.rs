use anyhow::Result;
use indicatif::MultiProgress;
use log::{debug, error, info, warn};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::cli::Args;
use crate::makemkv::{DiscInfo, Drive, MakeMKV, ProgressUpdate, Title};
use crate::output::RipProgressTracker;
use crate::transfer::move_rip_dir;
use crate::util::get_incremented_dir;
use crate::{LockState, notifications};

pub async fn discover_and_spawn(
    makemkv: &Arc<MakeMKV>,
    multi_progress: Option<&MultiProgress>,
    state: &Arc<LockState>,
    args: &Args,
    cancellation_token: &mut CancellationToken,
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
        let cloned_cancellation_token = cancellation_token.clone();
        let drive_index = drive.index;

        tokio::spawn(async move {
            select! {
                _ = cloned_cancellation_token.cancelled() => {
                    info!("Drive {}: Shutdown signal received.", drive_index);
                }
                _ = drive_task_loop(drive, makemkv, multi_progress, state, args) => {}
            }
        });
    }

    Ok(())
}

pub async fn handle_sigusr1(
    makemkv: &Arc<MakeMKV>,
    multi_progress: Option<&MultiProgress>,
    state: &Arc<LockState>,
    args: &Args,
    cancellation_token: CancellationToken,
) -> Result<CancellationToken> {
    // Reconfiguration will wait until the next opportunity to complete, so we don't need to
    // queue multiple reconfigurations.
    let _reconfig_lock = match state.reconfiguration_active.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            warn!("Reconfiguration already queued. Ignoring SIGUSR1.");
            return Ok(cancellation_token);
        }
    };
    // 1) Wait for all rips to complete. Tokio RwLock will block new readers until this write lock
    //    is granted.
    let _lock = state.rips_active.write().await;

    // 2) Stop existing drive tasks
    info!("Stopping existing drive tasks...");
    cancellation_token.cancel();

    // 3) Drop rip lock before spawning new tasks
    // We don't need to hold it while discovering/spawning,
    // and we definitely don't want to hold it if spawn takes time.
    // The old tasks are gone, so no rips are happening.
    drop(_lock);

    // 4) Spawn new rip threads
    let mut new_cancellation_token = CancellationToken::new();
    discover_and_spawn(
        makemkv,
        multi_progress,
        state,
        args,
        &mut new_cancellation_token,
    )
    .await?;

    info!("Reconfiguration complete.");
    Ok(new_cancellation_token)
}

async fn drive_task_loop(
    drive: Drive,
    makemkv: Arc<MakeMKV>,
    multi_progress: Option<MultiProgress>,
    state: Arc<LockState>,
    args: Args,
) {
    let drive_index = drive.index;
    info!(
        "Drive task started for drive {} ({}) - State: {:?}, Disc: {}",
        drive_index, drive.drive_name, drive.state, drive.disc_name
    );

    let mut last_disc_signature: Option<String> = None;
    let cancellation_token = CancellationToken::new();

    loop {
        tokio::time::sleep(Duration::from_secs(args.poll_interval)).await;

        debug!("Drive {}: Polling for disc info...", drive_index);
        let (disc_info, _titles) = poll_disc_info(&makemkv, drive_index).await;

        if disc_info.disc_type.is_none() {
            debug!("Drive {}: No disc detected.", drive_index);
            continue;
        }

        let current_signature = disc_info.get_signature();
        let is_new_disc = last_disc_signature
            .as_ref()
            .map_or(true, |s| s != &current_signature);

        if is_new_disc {
            process_new_disc(
                &makemkv,
                multi_progress.as_ref(),
                &drive,
                &disc_info,
                &state,
                &args,
                &cancellation_token,
            )
            .await;
            last_disc_signature = Some(current_signature);
        } else {
            info!(
                "Drive {}: Disc present but already processed ({}).",
                drive_index, current_signature
            );
        }
    }
}

/// Polls `get_disc_info` with a 60-second timeout, retrying on error or timeout until a
/// successful response is received.
async fn poll_disc_info(makemkv: &MakeMKV, drive_index: usize) -> (DiscInfo, Vec<Title>) {
    loop {
        let poll_res =
            tokio::time::timeout(Duration::from_secs(300), makemkv.get_disc_info(drive_index))
                .await;

        match poll_res {
            Ok(Ok(disc_info)) => return disc_info,
            Ok(Err(e)) => {
                error!("Drive {}: Error getting disc info: {:?}", drive_index, e);
            }
            Err(_elapsed) => {
                warn!(
                    "Drive {}: Timeout getting disc info, retrying in 5s...",
                    drive_index
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Determines and creates the output directory for a rip.
fn prepare_output_dir(
    output_dir: &PathBuf,
    base_name: &str,
    drive_index: usize,
) -> Result<PathBuf> {
    let final_output_dir = get_incremented_dir(output_dir, base_name).map_err(|e| {
        error!(
            "Drive {}: Failed to determine output dir: {:?}",
            drive_index, e
        );
        e
    })?;

    std::fs::create_dir_all(&final_output_dir).map_err(|e| {
        error!(
            "Drive {}: Failed to create output dir: {:?}",
            drive_index, e
        );
        anyhow::Error::from(e)
    })?;

    Ok(final_output_dir)
}

/// Spawns a background task to move a successfully ripped directory and send a completion
/// notification.
fn spawn_post_rip_success(
    drive_index: usize,
    final_output_dir: PathBuf,
    base_name: String,
    completed_dir: Option<PathBuf>,
    cancellation_token: CancellationToken,
) {
    tokio::task::spawn(async move {
        select! {
            _ = cancellation_token.cancelled() => {
                info!("Drive {}: Shutdown signal received during move.", drive_index);
            }
            _ = move_rip_dir(drive_index, &final_output_dir, completed_dir.as_ref()) => {
                if let Some(p) = notifications::pushover() {
                    p.notify_rip_completed(drive_index, &base_name).await;
                }
            }
        }
    });
}

/// Spawns a background task to clean up or move a failed rip directory and send a failure
/// notification.
fn spawn_post_rip_failure(
    drive_index: usize,
    final_output_dir: PathBuf,
    base_name: String,
    completed_dir: Option<PathBuf>,
    failed_dir: Option<PathBuf>,
    cancellation_token: CancellationToken,
) {
    let move_future = async move {
        let is_empty = std::fs::read_dir(&final_output_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);
        if is_empty {
            let _ = std::fs::remove_dir(&final_output_dir);
        } else {
            // failed_dir wins; if absent but completed_dir is set, use
            // that (move all terminal-state rips there).
            let dest_dir = failed_dir.as_ref().or(completed_dir.as_ref());
            move_rip_dir(drive_index, &final_output_dir, dest_dir).await;
        }
        if let Some(p) = notifications::pushover() {
            p.notify_rip_failed(drive_index, &base_name).await;
        }
    };

    tokio::task::spawn(async move {
        select! {
            _ = cancellation_token.cancelled() => {
                info!("Drive {}: Shutdown signal received during move.", drive_index);
            }
            _ = move_future => {}
        }
    });
}

/// Handles a newly detected disc: prepares the output directory, runs the rip workflow, spawns
/// the appropriate post-rip task, and ejects the disc.
async fn process_new_disc(
    makemkv: &Arc<MakeMKV>,
    multi_progress: Option<&MultiProgress>,
    drive: &Drive,
    disc_info: &DiscInfo,
    state: &Arc<LockState>,
    args: &Args,
    cancellation_token: &CancellationToken,
) {
    let drive_index = drive.index;
    let base_name = disc_info.get_base_name();
    info!("Drive {}: New disc detected: {}", drive_index, base_name);

    // Acquire "Rip Permit" (Read Lock)
    // Reconfiguration will cancel this future, so this will block until
    // the future is dropped. We don't need to manually cancel the future
    // using try_read().
    let permit = state.rips_active.read().await;

    // Prepare output directory
    let final_output_dir = match prepare_output_dir(&args.output_dir, &base_name, drive_index) {
        Ok(dir) => dir,
        Err(_) => {
            drop(permit);
            return;
        }
    };

    if let Some(p) = notifications::pushover() {
        p.notify_rip_started(drive_index, &base_name, &final_output_dir)
            .await;
    }

    // Rip
    let rip_result = run_rip_workflow(
        makemkv,
        multi_progress,
        drive_index,
        &base_name,
        &final_output_dir,
        args.min_length,
    )
    .await;

    // Release "Rip Permit"
    drop(permit);

    match &rip_result {
        Ok(()) => {
            info!("Drive {}: Rip complete.", drive_index);
            spawn_post_rip_success(
                drive_index,
                final_output_dir,
                base_name,
                args.completed_dir.clone(),
                cancellation_token.clone(),
            );
        }
        Err(e) => {
            error!("Drive {}: Rip failed: {:?}", drive_index, e);
            spawn_post_rip_failure(
                drive_index,
                final_output_dir,
                base_name,
                args.completed_dir.clone(),
                args.failed_dir.clone(),
                cancellation_token.clone(),
            );
        }
    }

    // Eject
    info!("Drive {}: Ejecting...", drive_index);
    if let Err(e) = makemkv.eject_disc(drive).await {
        error!("Drive {}: Eject failed: {:?}", drive_index, e);
    }

    // We do NOT wait for removal here explicitly.
    // The loop will continue, sleep, poll again.
    // If the disc is still there (eject failed or slow), get_disc_info will return same signature.
    // is_new_disc will be false.
    // We will just loop and sleep until signature changes (disc removed -> None, or new disc).
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
