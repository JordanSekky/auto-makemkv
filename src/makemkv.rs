use anyhow::{Context, Result};
use log::debug;
use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Drive {
    pub index: usize,
    pub drive_name: String,
    pub disc_name: String,
    pub device_path: Option<String>,
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
        std::fs::create_dir_all(config_path.as_ref().unwrap())?;

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
        let output = Command::new("makemkvcon")
            .arg("-r")
            .arg("--noscan")
            .arg("info")
            .arg(format!("disc:{}", drive_index))
            .env("HOME", self.home())
            .kill_on_drop(true)
            .output()
            .await
            .context("Failed to execute makemkvcon info")?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            return Err(anyhow::anyhow!("makemkvcon info failed: {}", stderr));
        }

        let mut disc_info = DiscInfo {
            disc_type: None,
            disc_title: None,
            volume_name: None,
            meta_lang_code: None,
            meta_lang_name: None,
        };
        let mut titles: HashMap<usize, Title> = HashMap::new();

        for line in stdout.lines() {
            if line.starts_with("CINFO:") {
                parse_cinfo_line(line, &mut disc_info);
            } else if line.starts_with("TINFO:") {
                parse_tinfo_line(line, &mut titles);
            }
        }

        let mut sorted_titles: Vec<Title> = titles.into_values().collect();
        sorted_titles.sort_by_key(|t| t.id);

        Ok((disc_info, sorted_titles))
    }

    pub async fn rip_disc<F>(
        &self,
        drive_index: usize,
        output_dir: &PathBuf,
        min_length: u64,
        progress_callback: F,
    ) -> Result<()>
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
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn makemkvcon rip process")?;

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
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
                    progress_callback(ProgressUpdate::Message(msg));
                }
            } else {
                debug!("Unhandled line: {}", line);
            }
        }

        let status = child
            .wait()
            .await
            .context("Failed to wait for makemkvcon process")?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "makemkvcon rip process failed with exit code: {:?}",
                status.code()
            ));
        }

        Ok(())
    }

    pub async fn eject_disc(&self, drive: &Drive) -> Result<()> {
        // macOS: drutil eject
        // Linux: eject <device_path>
        // We are on darwin
        #[cfg(target_os = "macos")]
        {
            // drutil eject seems to eject the first drive or we can specify?
            // drutil eject -drive internal
            // But we might have external.
            // drutil uses a different enumeration.
            // Let's try "eject <device_path>" first if available, or fallback to drutil.
            // Actually, 'eject' command might not be available on macOS by default?
            // macOS has 'drutil'.
            // 'drutil eject' ejects the media.
            // If we have multiple drives, we need to know which one.
            // drutil list shows drives.
            // Maybe we can use 'diskutil eject <device_path>'?
            if let Some(path) = &drive.device_path {
                let status = Command::new("diskutil")
                    .arg("eject")
                    .arg(path)
                    .kill_on_drop(true)
                    .status()
                    .await;

                if let Ok(s) = status {
                    if s.success() {
                        return Ok(());
                    }
                }
            }

            // Fallback to drutil eject (might eject wrong drive if multiple)
            // TODO: Better multi-drive eject support on macOS
            let _ = Command::new("drutil")
                .arg("eject")
                .kill_on_drop(true)
                .status()
                .await;
        }

        #[cfg(target_os = "linux")]
        {
            if let Some(path) = &drive.device_path {
                let _ = Command::new("eject")
                    .arg(path)
                    .kill_on_drop(true)
                    .status()
                    .await;
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum ProgressUpdate {
    Progress(Progress),
    ProgressTitle(ProgressTitle),
    Message(String),
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

fn parse_msg_line(line: &str) -> Option<String> {
    // MSG:code,flags,count,message,format,param0,...
    let content = line.strip_prefix("MSG:")?;
    let parts = parse_csv(content);
    if parts.len() >= 4 {
        return Some(parts[3].clone());
    }
    None
}
