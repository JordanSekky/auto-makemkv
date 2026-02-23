use std::{fmt::Display, path::PathBuf};

use indicatif::style::ProgressTracker;

#[derive(Clone)]
pub struct FilesizeProgressTracker {
    output_dir: PathBuf,
    cur_file: Option<PathBuf>,
    length_bytes: u64,
    estimated_total_bytes: Option<f64>,
}

impl FilesizeProgressTracker {
    pub fn new(output_dir: PathBuf) -> Self {
        Self {
            output_dir,
            cur_file: None,
            length_bytes: 0,
            estimated_total_bytes: None,
        }
    }

    pub fn current_file(&self) -> Option<PathBuf> {
        self.cur_file.clone()
    }

    pub fn update(&mut self, pos: u64, len: Option<u64>) {
        self.cur_file = self.find_latest_file();
        if let Some(path) = &self.cur_file {
            self.length_bytes = std::fs::metadata(path).unwrap().len();
            self.estimated_total_bytes =
                len.map(|l| l as f64 * self.length_bytes as f64 / pos as f64);
        } else {
            self.length_bytes = 0;
            self.estimated_total_bytes = None;
        }
    }

    pub fn clear(&mut self) {
        self.cur_file = None;
        self.length_bytes = 0;
        self.estimated_total_bytes = None;
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

impl Display for FilesizeProgressTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];

        let mut display_size = self.length_bytes as f64;
        let mut display_total = self.estimated_total_bytes.unwrap_or(display_size);
        let mut unit = units[0];

        for u in &units {
            unit = u;
            if display_total < 1024.0 {
                break;
            }
            display_size /= 1024.0;
            display_total /= 1024.0;
        }

        if let Some(path) = &self.cur_file {
            write!(
                f,
                "{}: {:.2} {} / ~{:.2} {}",
                path.file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("Unknown"))
                    .to_string_lossy(),
                display_size,
                unit,
                display_total,
                unit
            )
        } else {
            Ok(())
        }
    }
}

impl ProgressTracker for FilesizeProgressTracker {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(self.clone())
    }

    fn tick(&mut self, state: &indicatif::ProgressState, _now: std::time::Instant) {
        self.update(state.pos(), state.len());
    }

    fn reset(&mut self, _state: &indicatif::ProgressState, _now: std::time::Instant) {
        self.clear();
    }

    fn write(&self, _state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write) {
        // Just delegate to Display implementation
        let _ = write!(w, "{self}");
    }
}
