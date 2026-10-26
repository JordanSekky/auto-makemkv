use anyhow::{Context, Result};
use indicatif::MultiProgress;
use log::{debug, error, info, warn};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::cli::Args;
use crate::device;
use crate::makemkv::{
    DiscInfo, Drive, MakeMKV, ProgressUpdate, RipReport, Title, count_titles_to_rip,
};
use crate::output::RipProgressTracker;
use crate::rip_outcome::{RipOutcome, evaluate_rip};
use crate::transfer::move_rip_dir;
use crate::util::{count_files, get_incremented_dir};
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
    for mut drive in drives {
        info!(
            "Drive {}: '{}' at {} ({:?})",
            drive.index,
            drive.drive_name,
            drive.device_path.as_deref().unwrap_or("<no device path>"),
            drive.state
        );
        identify_drive(&mut drive);

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
        let (disc_info, titles) = poll_disc_info(&makemkv, drive_index).await;

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
                &titles,
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

/// Spawns a background task to move a failed or partial rip directory and send a failure
/// notification. The directory is kept even if empty, so the failed rip leaves a trace.
fn spawn_post_rip_failure(
    drive_index: usize,
    final_output_dir: PathBuf,
    base_name: String,
    detail: String,
    completed_dir: Option<PathBuf>,
    failed_dir: Option<PathBuf>,
    cancellation_token: CancellationToken,
) {
    let move_future = async move {
        // failed_dir wins; if absent but completed_dir is set, use
        // that (move all terminal-state rips there).
        let dest_dir = failed_dir.as_ref().or(completed_dir.as_ref());
        move_rip_dir(drive_index, &final_output_dir, dest_dir).await;
        if let Some(p) = notifications::pushover() {
            p.notify_rip_failed(drive_index, &base_name, &detail).await;
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
    titles: &[Title],
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

    // The index and device path were assigned at discovery. If the kernel has renumbered this
    // drive since, MakeMKV's numbering may have changed too, so disc:{index} could now be a
    // different drive. Re-discover (once no rips are running) instead of ripping.
    if drive.identity.is_some() {
        match current_device_path(drive) {
            Ok(path) if drive.device_path.as_ref() == Some(&path) => {}
            Ok(path) => {
                warn!(
                    "Drive {}: Now at {} (was {}). Re-discovering drives before ripping {}.",
                    drive_index,
                    path,
                    drive.device_path.as_deref().unwrap_or("<none>"),
                    base_name
                );
                drop(permit);
                state.rediscovery_requested.notify_one();
                return;
            }
            Err(e) => {
                error!(
                    "Drive {}: Not ripping {}: {:#}. Send SIGUSR1 to re-discover drives.",
                    drive_index, base_name, e
                );
                drop(permit);
                if let Some(p) = notifications::pushover() {
                    p.notify_rip_failed(drive_index, &base_name, "drive not found")
                        .await;
                }
                return;
            }
        }
    }

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

    let outcome = match &rip_result {
        Ok(report) => {
            for title_error in &report.title_errors {
                error!("Drive {}: {}", drive_index, title_error);
            }
            evaluate_rip(
                report,
                count_titles_to_rip(titles, args.min_length),
                count_files(&final_output_dir),
            )
        }
        Err(e) => {
            error!("Drive {}: Rip failed: {:?}", drive_index, e);
            RipOutcome::Incomplete {
                titles_saved: 0,
                titles_expected: count_titles_to_rip(titles, args.min_length),
                reasons: vec![format!("{:#}", e)],
            }
        }
    };

    match outcome {
        RipOutcome::Complete { .. } => {
            info!(
                "Drive {}: Rip complete ({}).",
                drive_index,
                outcome.summary()
            );
            spawn_post_rip_success(
                drive_index,
                final_output_dir,
                base_name,
                args.completed_dir.clone(),
                cancellation_token.clone(),
            );
        }
        RipOutcome::Incomplete { .. } => {
            error!(
                "Drive {}: Rip incomplete ({}).",
                drive_index,
                outcome.summary()
            );
            spawn_post_rip_failure(
                drive_index,
                final_output_dir,
                base_name,
                outcome.summary(),
                args.completed_dir.clone(),
                args.failed_dir.clone(),
                cancellation_token.clone(),
            );
        }
    }

    // Eject. Leaving the disc in is harmless; ejecting the wrong drive aborts its rip, so
    // don't eject unless this drive can be found.
    match current_device_path(drive) {
        Ok(path) => {
            info!("Drive {}: Ejecting {}...", drive_index, path);
            match device::eject(&path).await {
                Ok(()) => info!("Drive {}: Ejected {}.", drive_index, path),
                Err(e) => error!("Drive {}: {:#}", drive_index, e),
            }
        }
        Err(e) => error!("Drive {}: Not ejecting: {:#}", drive_index, e),
    }

    // We do NOT wait for removal here explicitly.
    // The loop will continue, sleep, poll again.
    // If the disc is still there (eject failed or slow), get_disc_info will return same signature.
    // is_new_disc will be false.
    // We will just loop and sleep until signature changes (disc removed -> None, or new disc).
}

/// Records the drive's sysfs identity, so it can be found again if its device node is renumbered.
#[cfg(target_os = "linux")]
fn identify_drive(drive: &mut Drive) {
    let Some(path) = drive.device_path.as_deref().filter(|p| !p.is_empty()) else {
        warn!(
            "Drive {}: No device path; it won't be ejected.",
            drive.index
        );
        return;
    };
    match device::identify_unique(std::path::Path::new(device::SYSFS_ROOT), path) {
        Ok(identity) => {
            info!("Drive {}: {} is {}", drive.index, path, identity);
            drive.identity = Some(identity);
        }
        Err(e) => warn!(
            "Drive {}: Can't identify {} ({:#}); it won't be ejected or checked before rips.",
            drive.index, path, e
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn identify_drive(_drive: &mut Drive) {}

/// The drive's device path now, looked up by its identity rather than taken from discovery.
#[cfg(target_os = "linux")]
fn current_device_path(drive: &Drive) -> Result<String> {
    let identity = drive
        .identity
        .as_ref()
        .context("drive was not identified at discovery")?;
    device::current_device_path(std::path::Path::new(device::SYSFS_ROOT), identity)
}

/// Without sysfs there's nothing to re-check against, so this is the path from discovery.
#[cfg(not(target_os = "linux"))]
fn current_device_path(drive: &Drive) -> Result<String> {
    drive
        .device_path
        .clone()
        .filter(|p| !p.is_empty())
        .context("drive has no device path")
}

async fn run_rip_workflow(
    makemkv: &MakeMKV,
    multi_progress: Option<&MultiProgress>,
    drive_index: usize,
    base_name: &str,
    output_dir: &std::path::PathBuf,
    min_length: u64,
) -> Result<RipReport> {
    info!(
        "Drive {}: Ripping {} to {:?}",
        drive_index, base_name, output_dir
    );

    let tracker = RipProgressTracker::new(multi_progress, drive_index, base_name, output_dir);
    let tracker_clone = tracker.clone();

    let report = makemkv
        .rip_disc(
            drive_index,
            output_dir,
            min_length,
            move |update| match update {
                ProgressUpdate::Progress(p) => {
                    tracker_clone.update_progress(&p);
                }
                ProgressUpdate::Message(msg) => {
                    info!(
                        "Drive {}: MakeMKV: [{}] {}",
                        drive_index, msg.code, msg.text
                    );
                }
                ProgressUpdate::ProgressTitle(progress_title) => {
                    tracker_clone.update_title(&progress_title);
                }
            },
        )
        .await?;

    tracker.finish();
    Ok(report)
}
