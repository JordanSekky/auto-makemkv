use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{Log, Metadata, Record, SetLoggerError, info};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::filesize_progress_tracker::FilesizeProgressTracker;
use crate::makemkv::{Progress, ProgressTitle, ProgressTitleType};

struct IndicatifLogger {
    inner: env_logger::Logger,
    multi: Option<MultiProgress>,
}

impl Log for IndicatifLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            if let Some(multi) = &self.multi {
                // Format similar to env_logger: [timestamp level target] message
                let now = Local::now().format("%Y-%m-%dT%H:%M:%SZ");
                let level = record.level();
                let target = record.target();
                let msg = format!("[{} {} {}] {}", now, level, target, record.args());

                // Print above progress bars
                let _ = multi.println(msg);
            } else {
                // Not a terminal or no progress bars, route to env_logger
                self.inner.log(record);
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

pub fn init_logger(multi: Option<MultiProgress>) -> Result<(), SetLoggerError> {
    let inner =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let max_level = inner.filter();

    let logger = IndicatifLogger { inner, multi };

    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(max_level);
    Ok(())
}

#[derive(Clone)]
pub struct RipProgressTracker {
    // If present, we use indicatif
    current_bar: Option<ProgressBar>,
    total_bar: Option<ProgressBar>,

    // If bars are None, we use this state for logging
    state: Option<Arc<Mutex<TrackerState>>>,

    // Store context for indicatif messages
    drive_index: usize,
    base_name: String,
}

struct TrackerState {
    last_log_time: Instant,
    current_title: String,
    total_title: String,
    current_pos: u64,
    total_pos: u64,
    max_pos: u64,
    filesize_tracker: FilesizeProgressTracker,
}

impl TrackerState {
    pub fn update_filesize(&mut self) {
        self.filesize_tracker
            .update(self.current_pos, Some(self.max_pos));
    }
}

impl RipProgressTracker {
    pub fn new(
        multi: Option<&MultiProgress>,
        drive_index: usize,
        base_name: &str,
        output_dir: &PathBuf,
    ) -> Self {
        if let Some(multi) = multi {
            let current_bar = create_current_progress_bar(
                multi,
                100,
                &format!("Drive {}: Ripping {}...", drive_index, base_name),
                output_dir,
            );
            let total_bar = create_total_progress_bar(
                multi,
                100,
                &format!("Drive {}: Ripping {}...", drive_index, base_name),
                &current_bar,
            );
            Self {
                current_bar: Some(current_bar),
                total_bar: Some(total_bar),
                state: None,
                drive_index,
                base_name: base_name.to_string(),
            }
        } else {
            Self {
                current_bar: None,
                total_bar: None,
                state: Some(Arc::new(Mutex::new(TrackerState {
                    last_log_time: Instant::now() - Duration::from_secs(10), // Force first log
                    current_title: String::new(),
                    total_title: String::new(),
                    current_pos: 0,
                    total_pos: 0,
                    max_pos: 100, // Default
                    filesize_tracker: FilesizeProgressTracker::new(output_dir.clone()),
                }))),
                drive_index,
                base_name: base_name.to_string(),
            }
        }
    }

    pub fn update_progress(&self, p: &Progress) {
        if let (Some(cb), Some(tb)) = (&self.current_bar, &self.total_bar) {
            cb.set_length(p.max);
            tb.set_length(p.max);
            cb.set_position(p.current);
            tb.set_position(p.total);
        } else if let Some(state) = &self.state {
            let mut state = state.lock().unwrap();
            state.current_pos = p.current;
            state.total_pos = p.total;
            state.max_pos = p.max;
            state.filesize_tracker.update(p.current, Some(p.max));
            self.maybe_log(&mut state);
        }
    }

    pub fn update_title(&self, t: &ProgressTitle) {
        if let (Some(cb), Some(tb)) = (&self.current_bar, &self.total_bar) {
            match t.title_type {
                ProgressTitleType::Current => {
                    cb.set_message(format!("Drive {}: {}", self.drive_index, t.name));
                }
                ProgressTitleType::Total => {
                    tb.set_message(format!("Drive {}: {}", self.drive_index, t.name));
                }
            }
        } else if let Some(state) = &self.state {
            let mut state = state.lock().unwrap();
            match t.title_type {
                ProgressTitleType::Current => state.current_title = t.name.clone(),
                ProgressTitleType::Total => {
                    state.total_title = t.name.clone();
                }
            }
            state.update_filesize();
            // Force a log, because the title has changed.
            self.log(&mut state);
        }
    }

    pub fn finish(&self) {
        if let (Some(cb), Some(tb)) = (&self.current_bar, &self.total_bar) {
            cb.finish_and_clear();
            tb.finish_with_message(format!(
                "Drive {}: Finished ripping {}",
                self.drive_index, self.base_name
            ));
        } else if let Some(_) = &self.state {
            info!(
                "Drive {}: Finished ripping {}",
                self.drive_index, self.base_name
            );
        }
    }

    fn maybe_log(&self, state: &mut TrackerState) {
        if state.last_log_time.elapsed() >= Duration::from_secs(5) {
            self.log(state);
        }
    }

    fn log(&self, state: &mut TrackerState) {
        let current_pct = if state.max_pos > 0 {
            (state.current_pos as f64 / state.max_pos as f64) * 100.0
        } else {
            0.0
        };
        let total_pct = if state.max_pos > 0 {
            (state.total_pos as f64 / state.max_pos as f64) * 100.0
        } else {
            0.0
        };

        if let Some(_) = state.filesize_tracker.current_file() {
            info!(
                "Drive {}: Progress: {:.1}% (Current: {:.1}% {}) - {} - {}",
                self.drive_index,
                total_pct,
                current_pct,
                state.filesize_tracker,
                state.total_title,
                state.current_title
            );
        } else {
            info!(
                "Drive {}: Progress: {:.1}% (Current: {:.1}%) - {} - {}",
                self.drive_index, total_pct, current_pct, state.total_title, state.current_title
            );
        }
        state.last_log_time = Instant::now();
    }
}

pub fn create_total_progress_bar(
    multi: &MultiProgress,
    len: u64,
    msg: &str,
    current_bar: &ProgressBar,
) -> ProgressBar {
    let pb = multi.insert_after(current_bar, ProgressBar::new(len));
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent_precise}% ({eta}) {msg}")
        .unwrap()
        .progress_chars("#>-"));
    pb.set_message(msg.to_string());
    pb
}

pub fn create_current_progress_bar(
    multi: &MultiProgress,
    len: u64,
    msg: &str,
    output_dir: &PathBuf,
) -> ProgressBar {
    let pb = multi.add(ProgressBar::new(len));
    pb.set_style(ProgressStyle::default_bar()
        .with_key("filesize", FilesizeProgressTracker::new(output_dir.clone()))
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}% ({eta}) {filesize} {msg}")
        .unwrap()
        .progress_chars("#>-"));
    pb.set_message(msg.to_string());
    pb
}
