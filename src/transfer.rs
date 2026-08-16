use anyhow::Result;
use log::error;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs::File;
use tokio::select;
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
    for entry in WalkDir::new(src).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                error!("Drive {}: Failed to walk {:?}: {:?}", drive_index, src, e);
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel_path = entry.path().strip_prefix(src.parent().unwrap()).unwrap();
        let dest_path = dest_dir.join(rel_path);
        let src_path = entry.into_path();
        if let Err(e) = move_file_with_progress(&src_path, &dest_path).await {
            error!(
                "Drive {}: Failed to move file {:?} to {:?}: {:?}",
                drive_index, src_path, dest_path, e
            );
        };
    }
    // Clean up now-empty directories left behind in src, bottom-up. Any
    // directory that still contains a file (because that file failed to
    // move above) is left in place rather than reported as an error.
    for entry in WalkDir::new(src)
        .contents_first(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir() {
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

async fn move_file_with_progress(src: &PathBuf, dest: &PathBuf) -> Result<()> {
    // Safe unwrap: src is guaranteed to be a file in a directory.
    let display_path = src
        .strip_prefix(src.parent().unwrap().parent().unwrap())
        .unwrap();
    let src_size = std::fs::metadata(src)?.len();
    let mut src_file = File::open(src).await?;
    src_file.set_max_buf_size(128 * 1024 * 1024); // 128MB
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut dest_file = File::create(dest).await?;
    dest_file.set_max_buf_size(128 * 1024 * 1024); // 128MB
    let src_reader = AsyncReadWithSizeImpl::new(src_file, src_size as usize);
    let dest_writer = AsyncWriteWithSizeImpl::new(dest_file, src_size as usize);

    // Clone the Arc before moving the reader into the task so we can poll it for progress.
    let total_read = src_reader.total_read();
    let total_size = src_reader.total_size();

    let mut join_handle = tokio::task::spawn(async move {
        let mut src_reader = src_reader;
        let mut dest_writer = dest_writer;
        tokio::io::copy(&mut src_reader, &mut dest_writer).await?;
        Ok::<(), std::io::Error>(())
    });
    let mut progress_interval = tokio::time::interval(Duration::from_secs(1));
    let copy_result = loop {
        select! {
            result = &mut join_handle => {
                break result?;
            }
            _ = progress_interval.tick() => {
                let read = total_read.load(std::sync::atomic::Ordering::Relaxed);
                let size = total_size.load(std::sync::atomic::Ordering::Relaxed);
                log::info!(
                    "{}: {} / {}",
                    display_path.to_string_lossy(),
                    fmt_bytes(read),
                    fmt_bytes(size),
                );
            }
        }
    };
    // Only delete the source once the copy has actually succeeded, so a
    // failed/partial copy never leaves us with no complete copy anywhere.
    copy_result?;
    tokio::fs::remove_file(src).await?;
    log::info!(
        "{}: File move completed ({}).",
        display_path.to_string_lossy(),
        fmt_bytes(total_size.load(std::sync::atomic::Ordering::Relaxed))
    );
    Ok(())
}
