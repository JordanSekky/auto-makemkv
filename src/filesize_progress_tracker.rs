use std::{ffi::OsStr, path::PathBuf};

use indicatif::style::ProgressTracker;

#[derive(Clone)]
pub struct FilesizeProgressTracker {
    output_dir: PathBuf,
    cur_file: Option<PathBuf>,
    length: u64,
}

impl FilesizeProgressTracker {
    pub fn new(output_dir: PathBuf) -> Self {
        Self {
            output_dir,
            cur_file: None,
            length: 0,
        }
    }

    fn find_latest_file(&mut self) -> Option<PathBuf> {
        if let Ok(entries) = std::fs::read_dir(&self.output_dir) {
            let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        if let Ok(modified) = metadata.modified() {
                            match &latest {
                                Some((_, latest_time)) if &modified > latest_time => {
                                    latest = Some((entry.path(), modified));
                                }
                                None => {
                                    latest = Some((entry.path(), modified));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            if let Some((path, _)) = latest {
                self.cur_file = Some(path.clone());
                return Some(path);
            }
        }
        None
    }
}

impl ProgressTracker for FilesizeProgressTracker {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(self.clone())
    }

    fn tick(&mut self, _state: &indicatif::ProgressState, _now: std::time::Instant) {
        self.cur_file = self.find_latest_file();
        if let Some(path) = &self.cur_file {
            self.length = std::fs::metadata(path).unwrap().len();
        } else {
            self.length = 0;
        }
    }

    fn reset(&mut self, _state: &indicatif::ProgressState, _now: std::time::Instant) {
        self.length = 0;
        self.cur_file = None;
    }

    fn write(&self, state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write) {
        let units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
        let size = self.length as f64;
        let mut unit = units[0];
        let total_factor = state
            .len()
            .map(|l| l as f64 / state.pos() as f64)
            .unwrap_or(1.0);
        let mut display_size = size;
        let mut display_total = size * total_factor;

        for u in &units {
            unit = u;
            if display_total < 1024.0 {
                break;
            }
            display_size /= 1024.0;
            display_total /= 1024.0;
        }
        match &self.cur_file {
            Some(path) => {
                // We know current file and can estimate total
                write!(
                    w,
                    "{}: {:.2} {} / ~{:.2} {}",
                    path.file_name()
                        .unwrap_or(OsStr::new("Unknown"))
                        .to_string_lossy(),
                    display_size,
                    unit,
                    display_total,
                    unit
                )
                .unwrap();
            }
            None => (),
        }
    }
}
