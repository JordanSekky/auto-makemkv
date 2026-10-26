use anyhow::{Context, Result};
use log::{debug, info, warn};
use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::select;

use crate::device::DeviceIdentity;

#[derive(Debug, Clone)]
pub struct Drive {
    pub index: usize,
    pub drive_name: String,
    pub disc_name: String,
    // Device path when the drive was discovered. The kernel can renumber it later, so look the
    // drive up by `identity` before using it.
    pub device_path: Option<String>,
    pub identity: Option<DeviceIdentity>,
    pub state: DriveState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveState {
    Inserted,    // 2
    NoDrive,     // 256
    EmptyClosed, // 0
    EmptyOpen,   // 1
    Loading,     // 3
    Unknown(i32),
}
impl DriveState {
    pub fn is_valid(&self) -> bool {
        match self {
            DriveState::Inserted => true,
            DriveState::EmptyClosed => true,
            DriveState::EmptyOpen => true,
            DriveState::Loading => true,
            DriveState::NoDrive => false,
            DriveState::Unknown(_) => false,
        }
    }
}

impl From<i32> for DriveState {
    fn from(code: i32) -> Self {
        match code {
            2 => DriveState::Inserted,
            256 => DriveState::NoDrive,
            0 => DriveState::EmptyClosed,
            1 => DriveState::EmptyOpen,
            3 => DriveState::Loading,
            c => DriveState::Unknown(c),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscInfo {
    pub disc_type: Option<String>,      // CINFO:1 (6209 = BRAY_TYPE_DISK)
    pub disc_title: Option<String>,     // CINFO:2 or CINFO:30
    pub volume_name: Option<String>,    // CINFO:32
    pub meta_lang_code: Option<String>, // CINFO:28
    pub meta_lang_name: Option<String>, // CINFO:29
}

impl DiscInfo {
    pub fn get_base_name(&self) -> String {
        // Prefer volume name, then disc title, then "Unknown_Disc"
        let raw_name = self
            .volume_name
            .as_ref()
            .or(self.disc_title.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("Unknown_Disc");

        sanitize_filename(raw_name)
    }

    // Generate a signature to identify the disc
    pub fn get_signature(&self) -> String {
        // Combine volume name and disc title
        let vol = self.volume_name.as_deref().unwrap_or("");
        let title = self.disc_title.as_deref().unwrap_or("");
        format!("{}:{}", vol, title)
    }
}

fn sanitize_filename(name: &str) -> String {
    name.replace(['/', ':', '\\', ' '], "_")
        .trim_matches('_')
        .to_string()
}

#[derive(Debug, Clone)]
pub struct Title {
    pub id: usize,
    pub name: Option<String>,
    pub chapter_count: Option<u32>,
    pub duration: Option<String>,
    pub disk_size: Option<String>,
    pub disk_size_bytes: Option<u64>,
    pub output_filename: Option<String>,
}

impl Title {
    // TINFO:9 is formatted as "h:mm:ss".
    pub fn duration_secs(&self) -> Option<u64> {
        let mut secs = 0;
        for part in self.duration.as_deref()?.split(':') {
            secs = secs * 60 + part.trim().parse::<u64>().ok()?;
        }
        Some(secs)
    }
}

/// Number of titles a rip with `--minlength=min_length` is expected to save. Titles with an
/// unknown duration are counted, so a rip that loses them is not mistaken for a clean one.
pub fn count_titles_to_rip(titles: &[Title], min_length: u64) -> u32 {
    titles
        .iter()
        .filter(|t| t.duration_secs().map_or(true, |d| d >= min_length))
        .count() as u32
}

// MSG codes from `makemkvcon -r`, checked against MakeMKV v2.0.0's message catalog and output.
pub const MSG_TITLE_SAVE_FAILED: u32 = 5003; // "Failed to save title %1 to file %2"
pub const MSG_TITLES_SAVED_FAILED: u32 = 5004; // "%1 titles saved, %2 failed"
pub const MSG_TITLES_SAVED: u32 = 5005; // "%1 titles saved"
pub const MSG_SAVING_TITLES: u32 = 5014; // "Saving %1 titles into directory %2"
pub const MSG_COPY_COMPLETE: u32 = 5036; // "Copy complete. %1 titles saved."
pub const MSG_COPY_COMPLETE_FAILED: u32 = 5037; // "Copy complete. %1 titles saved, %2 failed."

#[derive(Debug, Clone)]
pub struct Message {
    pub code: u32,
    pub _flags: u32,
    pub text: String,
    pub params: Vec<String>,
}

impl Message {
    fn param_u32(&self, i: usize) -> Option<u32> {
        self.params.get(i)?.parse().ok()
    }
}

/// What a `makemkvcon mkv` run reported. makemkvcon exits 0 even when titles fail, so the
/// outcome has to come from its messages.
#[derive(Debug, Clone, Default)]
pub struct RipReport {
    pub exit_code: Option<i32>,
    pub titles_to_save: Option<u32>,
    pub titles_saved: Option<u32>,
    pub titles_failed: Option<u32>,
    pub title_errors: Vec<String>,
}

impl RipReport {
    pub fn observe(&mut self, msg: &Message) {
        match msg.code {
            MSG_SAVING_TITLES => self.titles_to_save = msg.param_u32(0),
            MSG_TITLES_SAVED | MSG_COPY_COMPLETE => {
                self.titles_saved = msg.param_u32(0);
                self.titles_failed = Some(0);
            }
            MSG_TITLES_SAVED_FAILED | MSG_COPY_COMPLETE_FAILED => {
                self.titles_saved = msg.param_u32(0);
                self.titles_failed = msg.param_u32(1);
            }
            MSG_TITLE_SAVE_FAILED => self.title_errors.push(msg.text.clone()),
            _ => {}
        }
    }
}

pub struct MakeMKV {
    key: Option<String>,
    settings_dir: Option<PathBuf>,
}

impl MakeMKV {
    pub fn new(key: Option<&str>, settings_dir: Option<&PathBuf>) -> Self {
        Self {
            key: key.map(|k| k.to_string()),
            settings_dir: settings_dir.map(|s| s.clone()),
        }
    }

    pub fn init(&self) -> Result<()> {
        // Create the configuration file if it doesn't exist
        // Path: <output_dir>/settings.conf

        use std::fs;
        use std::io::Write;

        let config_path = self
            .settings_dir
            .as_ref()
            .map(|s| s.join(".MakeMKV/settings.conf"));
        std::fs::create_dir_all(config_path.as_ref().unwrap().parent().unwrap())?;

        if config_path.is_some() && !config_path.clone().unwrap().exists() && self.key.is_some() {
            let mut file = fs::File::create(config_path.as_ref().unwrap())?;
            let conf = r#"
app_Key = "<KEY_PLACEHOLDER>"
app_ShowDebug = "1"
io_ErrorRetryCount = "10"
io_RBufSizeMB = "1024"
"#
            .replace("<KEY_PLACEHOLDER>", &self.key.as_ref().unwrap());

            file.write_all(conf.trim_start().as_bytes())?;
        }

        Ok(())
    }

    // Use settings directory if provided, otherwise use home directory.
    // This way if users are running this locally, they can use their default MakeMKV settings.
    fn home(&self) -> PathBuf {
        if self.settings_dir.is_some() {
            self.settings_dir.clone().unwrap()
        } else {
            PathBuf::from(std::env::var("HOME").unwrap())
        }
    }

    pub async fn discover_drives(&self) -> Result<Vec<Drive>> {
        // makemkvcon -r --noscan --cache=1 info disc:9999
        let output = Command::new("makemkvcon")
            .arg("-r")
            .arg("--noscan")
            .arg("--cache=1")
            .arg("info")
            .arg("disc:9999")
            .env("HOME", self.home())
            .kill_on_drop(true)
            .output()
            .await
            .context("Failed to execute makemkvcon. Is it installed and on PATH?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("makemkvcon failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut drives = Vec::new();

        for line in stdout.lines() {
            if line.starts_with("DRV:") {
                if let Some(drive) = parse_drv_line(line) {
                    drives.push(drive);
                }
            }
        }

        Ok(drives)
    }

    pub async fn get_disc_info(&self, drive_index: usize) -> Result<(DiscInfo, Vec<Title>)> {
        // makemkvcon -r --noscan --cache=1 info disc:N
        let mut child = Command::new("makemkvcon")
            .arg("-r")
            .arg("--noscan")
            .arg("info")
            .arg(format!("disc:{}", drive_index))
            .env("HOME", self.home())
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn makemkvcon info")?;

        let stderr = child.stderr.take().context("Failed to capture stderr")?;
        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let mut disc_info = DiscInfo {
            disc_type: None,
            disc_title: None,
            volume_name: None,
            meta_lang_code: None,
            meta_lang_name: None,
        };
        let mut titles: HashMap<usize, Title> = HashMap::new();

        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut stdout_lines = BufReader::new(stdout).lines();

        let status = loop {
            select! {
                line = stderr_lines.next_line() => {
                    if let Ok(Some(line)) = line {
                        debug!("Drive {}: Stderr: {}", drive_index, line);
                    }
                }
                line = stdout_lines.next_line() => {
                    if let Ok(Some(line)) = line {
                if line.starts_with("CINFO:") {
                    parse_cinfo_line(&line, &mut disc_info);
                } else if line.starts_with("TINFO:") {
                    parse_tinfo_line(&line, &mut titles);
                } else {
                    debug!("Drive {}: Unhandled line: {}", drive_index, line);
                }
                    }
                }
                status = child.wait() => {
                    break status.expect("Failed to wait for makemkvcon info");
                }
            }
        };

        let mut sorted_titles: Vec<Title> = titles.into_values().collect();
        sorted_titles.sort_by_key(|t| t.id);

        if !status.success() {
            return Err(anyhow::anyhow!("makemkvcon info failed: {}", status));
        }

        Ok((disc_info, sorted_titles))
    }

    pub async fn rip_disc<F>(
        &self,
        drive_index: usize,
        output_dir: &PathBuf,
        min_length: u64,
        progress_callback: F,
    ) -> Result<RipReport>
    where
        F: Fn(ProgressUpdate) + Send + Sync + 'static,
    {
        // mkv disc:N all <destination_folder> --noscan -r --progress=-same --minlength=seconds
        // Assuming 'all' for now as per plan
        let mut child = Command::new("makemkvcon")
            .arg("-r")
            .arg("--cache=1024")
            .arg("--noscan")
            .arg("--progress=-same")
            .arg(format!("--minlength={}", min_length))
            .arg("mkv")
            .arg(format!("disc:{}", drive_index))
            .arg("all")
            .arg(output_dir)
            .env("HOME", self.home())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn makemkvcon rip process")?;

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut report = RipReport::default();

        let status = loop {
            select! {
                line = stderr_lines.next_line() => {
                    if let Ok(Some(line)) = line {
                        warn!("Drive {}: Stderr: {}", drive_index, line);
                    }
                }
                line = stdout_lines.next_line() => {
                    if let Ok(Some(line)) = line {
                        if line.starts_with("PRGC:") || line.starts_with("PRGT:") {
                            if let Some(progress_title) = parse_prgc_prgt_line(&line) {
                                progress_callback(ProgressUpdate::ProgressTitle(progress_title));
                            }
                        } else if line.starts_with("PRGV:") {
                            if let Some(progress) = parse_prgv_line(&line) {
                                progress_callback(ProgressUpdate::Progress(progress));
                            }
                        } else if line.starts_with("MSG:") {
                            if let Some(msg) = parse_msg_line(&line) {
                                report.observe(&msg);
                                progress_callback(ProgressUpdate::Message(msg));
                            }
                        } else {
                            debug!("Drive {}: Unhandled line: {}", drive_index, line);
                        }
                    }
                }
                status = child.wait() => {
                    break status.expect("Failed to wait for makemkvcon process");
                }
            }
        };
        // Drain output still buffered after exit, which includes the end-of-run summary. Bounded
        // in case a helper process inherited stdout and keeps it open.
        let drain = async {
            while let Ok(Some(line)) = stdout_lines.next_line().await {
                if let Some(msg) = parse_msg_line(&line) {
                    report.observe(&msg);
                    progress_callback(ProgressUpdate::Message(msg));
                }
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(10), drain)
            .await
            .is_err()
        {
            warn!("Drive {}: Timed out reading makemkvcon output", drive_index);
        }
        info!("Drive {}: makemkvcon exited with {}", drive_index, status);
        report.exit_code = status.code();

        Ok(report)
    }
}

#[derive(Debug)]
pub enum ProgressUpdate {
    Progress(Progress),
    ProgressTitle(ProgressTitle),
    Message(Message),
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub current: u64,
    pub total: u64,
    pub max: u64,
}

#[derive(Debug, Clone)]
pub enum ProgressTitleType {
    Current,
    Total,
}

#[derive(Debug, Clone)]
pub struct ProgressTitle {
    pub title_type: ProgressTitleType,
    pub _code: u64,
    pub _id: u64,
    pub name: String,
}

// Simple CSV parser that handles quoted strings
fn parse_csv(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current_part = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    // Escaped quote
                    current_part.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                parts.push(current_part);
                current_part = String::new();
            }
            _ => {
                current_part.push(c);
            }
        }
    }
    parts.push(current_part);
    parts
}

fn parse_drv_line(line: &str) -> Option<Drive> {
    // DRV:index,visible,enabled,flags,"drive name","disc name","device path"
    let content = line.strip_prefix("DRV:")?;
    let parts = parse_csv(content);
    if parts.len() < 6 {
        return None;
    }

    let index = parts[0].parse().ok()?;
    let state_code: i32 = parts[1].parse().ok()?;
    // parts[2] = enabled, parts[3] = flags
    let drive_name = parts[4].clone();

    let disc_name = parts[5].clone();
    let device_path = if parts.len() > 6 {
        Some(parts[6].clone())
    } else {
        None
    };
    let drive_state = DriveState::from(state_code);
    if !drive_state.is_valid() {
        return None;
    }

    Some(Drive {
        index,
        drive_name,
        disc_name,
        device_path,
        identity: None,
        state: DriveState::from(state_code),
    })
}

fn parse_cinfo_line(line: &str, info: &mut DiscInfo) {
    // CINFO:id,code,value
    let content = match line.strip_prefix("CINFO:") {
        Some(c) => c,
        None => return,
    };
    let parts = parse_csv(content);
    if parts.len() < 3 {
        return;
    }

    let id: i32 = parts[0].parse().unwrap_or(-1);
    let value = parts[2].clone();

    match id {
        1 => info.disc_type = Some(value),
        2 => info.disc_title = Some(value),
        30 => {
            if info.disc_title.is_none() {
                info.disc_title = Some(value)
            }
        }
        32 => info.volume_name = Some(value),
        28 => info.meta_lang_code = Some(value),
        29 => info.meta_lang_name = Some(value),
        _ => {}
    }
}

fn parse_tinfo_line(line: &str, titles: &mut HashMap<usize, Title>) {
    // TINFO:titleId,id,code,value
    let content = match line.strip_prefix("TINFO:") {
        Some(c) => c,
        None => return,
    };
    let parts = parse_csv(content);
    if parts.len() < 4 {
        return;
    }

    let title_id: usize = parts[0].parse().unwrap_or(0);
    let id: i32 = parts[1].parse().unwrap_or(-1);
    let value = parts[3].clone();

    let title = titles.entry(title_id).or_insert(Title {
        id: title_id,
        name: None,
        chapter_count: None,
        duration: None,
        disk_size: None,
        disk_size_bytes: None,
        output_filename: None,
    });

    match id {
        2 => title.name = Some(value),
        8 => title.chapter_count = value.parse().ok(),
        9 => title.duration = Some(value),
        10 => title.disk_size = Some(value),
        11 => title.disk_size_bytes = value.parse().ok(),
        27 => title.output_filename = Some(value),
        _ => {}
    }
}

fn parse_prgv_line(line: &str) -> Option<Progress> {
    // PRGV:current,total,max
    let content = line.strip_prefix("PRGV:")?;
    let parts: Vec<&str> = content.split(',').collect();
    if parts.len() < 3 {
        return None;
    }

    Some(Progress {
        current: parts[0].parse().unwrap_or(0),
        total: parts[1].parse().unwrap_or(0),
        max: parts[2].parse().unwrap_or(0),
    })
}

fn parse_prgc_prgt_line(line: &str) -> Option<ProgressTitle> {
    // PRGC:current,total,max
    let content = Regex::new("PRGC\\:|PRGT\\:").unwrap().replace(line, "");
    let parts: Vec<&str> = content.split(',').collect();
    if parts.len() < 3 {
        return None;
    }

    Some(ProgressTitle {
        title_type: if line.starts_with("PRGC:") {
            ProgressTitleType::Current
        } else {
            ProgressTitleType::Total
        },
        _code: parts[0].parse().unwrap_or(0),
        _id: parts[1].parse().unwrap_or(0),
        name: parts[2].to_string(),
    })
}

fn parse_msg_line(line: &str) -> Option<Message> {
    // MSG:code,flags,count,message,format,param0,...
    let content = line.strip_prefix("MSG:")?;
    let mut parts = parse_csv(content);
    if parts.len() < 4 {
        return None;
    }
    let params = parts.split_off(5.min(parts.len()));
    Some(Message {
        code: parts[0].parse().ok()?,
        _flags: parts[1].parse().unwrap_or(0),
        text: parts[3].clone(),
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from makemkvcon v2.0.0 ripping a two-title DVD (paths shortened).
    const CLEAN_RIP: &[&str] = &[
        r#"MSG:5014,131072,2,"Saving 2 titles into directory file:///out","Saving %1 titles into directory %2","2","file:///out""#,
        r#"MSG:5005,128,1,"2 titles saved","%1 titles saved","2""#,
        r#"MSG:5036,260,1,"Copy complete. 2 titles saved.","Copy complete. %1 titles saved.","2""#,
    ];
    const PARTIAL_RIP: &[&str] = &[
        r#"MSG:5014,131072,2,"Saving 2 titles into directory file:///out","Saving %1 titles into directory %2","2","file:///out""#,
        r#"MSG:2019,0,2,"Error 'Posix error - Is a directory' occurred while creating '/out/A2_t01.mkv'","Error '%1' occurred while creating '%2'","Posix error - Is a directory","/out/A2_t01.mkv""#,
        r#"MSG:5003,0,2,"Failed to save title 1 to file /out/A2_t01.mkv","Failed to save title %1 to file %2","1","/out/A2_t01.mkv""#,
        r#"MSG:5004,128,2,"1 titles saved, 1 failed","%1 titles saved, %2 failed","1","1""#,
        r#"MSG:5037,516,2,"Copy complete. 1 titles saved, 1 failed.","Copy complete. %1 titles saved, %2 failed.","1","1""#,
    ];

    fn report_from(lines: &[&str]) -> RipReport {
        let mut report = RipReport::default();
        for line in lines {
            report.observe(&parse_msg_line(line).unwrap());
        }
        report
    }

    #[test]
    fn parses_msg_line() {
        let msg = parse_msg_line(PARTIAL_RIP[4]).unwrap();
        assert_eq!(msg.code, MSG_COPY_COMPLETE_FAILED);
        assert_eq!(msg.text, "Copy complete. 1 titles saved, 1 failed.");
        assert_eq!(msg.params, vec!["1", "1"]);

        let msg =
            parse_msg_line(r#"MSG:5010,0,0,"Failed to open disc","Failed to open disc""#).unwrap();
        assert_eq!(msg.code, 5010);
        assert!(msg.params.is_empty());
    }

    #[test]
    fn report_from_clean_rip() {
        let report = report_from(CLEAN_RIP);
        assert_eq!(report.titles_to_save, Some(2));
        assert_eq!(report.titles_saved, Some(2));
        assert_eq!(report.titles_failed, Some(0));
        assert!(report.title_errors.is_empty());
    }

    #[test]
    fn report_from_partial_rip() {
        let report = report_from(PARTIAL_RIP);
        assert_eq!(report.titles_to_save, Some(2));
        assert_eq!(report.titles_saved, Some(1));
        assert_eq!(report.titles_failed, Some(1));
        assert_eq!(
            report.title_errors,
            vec!["Failed to save title 1 to file /out/A2_t01.mkv"]
        );
    }

    #[test]
    fn report_without_summary() {
        let report = report_from(&CLEAN_RIP[..1]);
        assert_eq!(report.titles_saved, None);
        assert_eq!(report.titles_failed, None);
    }

    fn title(duration: Option<&str>) -> Title {
        Title {
            id: 0,
            name: None,
            chapter_count: None,
            duration: duration.map(String::from),
            disk_size: None,
            disk_size_bytes: None,
            output_filename: None,
        }
    }

    #[test]
    fn counts_titles_at_or_above_min_length() {
        assert_eq!(title(Some("1:02:03")).duration_secs(), Some(3723));
        let titles = [
            title(Some("0:00:20")),
            title(Some("0:02:00")),
            title(Some("0:44:10")),
            title(None),
        ];
        assert_eq!(count_titles_to_rip(&titles, 120), 3);
    }
}
