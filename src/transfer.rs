use anyhow::Result;
use log::error;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs::File;
use tokio::select;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::file::{AsyncReadWithSize, AsyncReadWithSizeImpl, AsyncWriteWithSizeImpl};
use crate::util::fmt_bytes;

pub async fn move_rip_dir(drive_index: usize, src: &PathBuf, dest_dir: Option<&PathBuf>) {
    let Some(dest_dir) = dest_dir else { return };
    if let Err(e) = std::fs::create_dir_all(dest_dir) {
        error!(
            "Drive {}: Failed to create destination dir {:?}: {:?}",
            drive_index, dest_dir, e
        );
        return;
    }
    let dest = dest_dir.join(src.file_name().unwrap());
    if let Ok(()) = std::fs::rename(src, &dest) {
        return;
    };
    for entry in WalkDir::new(src)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
    {
        let rel_path = entry.path().strip_prefix(src).unwrap();
        let dest_path = dest_dir.join(rel_path);
        let src_path = entry.into_path();
        if let Err(e) = move_file_with_progress(drive_index, &src_path, &dest_path).await {
            error!(
                "Drive {}: Failed to move file {:?} to {:?}: {:?}",
                drive_index, src_path, dest_path, e
            );
        };
    }
}

async fn move_file_with_progress(drive_index: usize, src: &PathBuf, dest: &PathBuf) -> Result<()> {
    let src_size = std::fs::metadata(src)?.len();
    let dest_size = std::fs::metadata(dest)?.len();
    let mut src_file = File::open(src).await?;
    src_file.set_max_buf_size(128 * 1024 * 1024); // 128MB
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut dest_file = File::create(dest).await?;
    dest_file.set_max_buf_size(128 * 1024 * 1024); // 128MB
    let src_reader = AsyncReadWithSizeImpl::new(src_file, src_size as usize);
    let dest_writer = AsyncWriteWithSizeImpl::new(dest_file, dest_size as usize);

    // Clone the Arc before moving the reader into the task so we can poll it for progress.
    let total_read = src_reader.total_read();
    let total_size = src_reader.total_size();

    let cancellation_token = CancellationToken::new();
    let join_handle = tokio::task::spawn(async move {
        let mut src_reader = src_reader;
        let mut dest_writer = dest_writer;
        select! {
            _ = cancellation_token.cancelled() => {
                return Err(anyhow::anyhow!("File move cancelled"));
            }
            _ = tokio::io::copy(&mut src_reader, &mut dest_writer) => {
                return Ok(());
            }
        }
    });
    let mut progress_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        progress_interval.tick().await;
        if join_handle.is_finished() {
            tokio::fs::remove_file(src).await?;
            log::info!(
                "Drive {}: File move completed ({}).",
                drive_index,
                fmt_bytes(total_size.load(std::sync::atomic::Ordering::Relaxed))
            );
            break;
        }
        let read = total_read.load(std::sync::atomic::Ordering::Relaxed);
        let size = total_size.load(std::sync::atomic::Ordering::Relaxed);
        log::info!(
            "Drive {}: {} / {}",
            drive_index,
            fmt_bytes(read),
            fmt_bytes(size),
        );
    }
    Ok(())
}
