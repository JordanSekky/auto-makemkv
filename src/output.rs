use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{Log, Metadata, Record, SetLoggerError};
use std::{io::IsTerminal, path::PathBuf};

use crate::filesize_progress_tracker::FilesizeProgressTracker;

struct IndicatifLogger {
    inner: env_logger::Logger,
    multi: MultiProgress,
}

impl Log for IndicatifLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            if std::io::stdout().is_terminal() {
                // Format similar to env_logger: [timestamp level target] message
                let now = Local::now().format("%Y-%m-%dT%H:%M:%SZ");
                let level = record.level();
                let target = record.target();
                let msg = format!("[{} {} {}] {}", now, level, target, record.args());

                // Print above progress bars
                let _ = self.multi.println(msg);
            } else {
                // Not a terminal, route to env_logger
                self.inner.log(record);
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

pub fn init_logger(multi: MultiProgress) -> Result<(), SetLoggerError> {
    let inner =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let max_level = inner.filter();

    let logger = IndicatifLogger { inner, multi };

    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(max_level);
    Ok(())
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
