use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Output directory for rips
    #[arg(
        short,
        long,
        env = "AUTO_MAKEMKV_OUTPUT_DIR",
        default_value = "./output"
    )]
    pub output_dir: PathBuf,

    /// MakeMKV settings directory (optional if running on a machine with makemkv already configured)
    #[arg(short, long, env = "AUTO_MAKEMKV_SETTINGS_DIR")]
    pub settings_dir: Option<PathBuf>,

    /// MakeMKV API key (optional if running on a machine with makemkv already configured)
    #[arg(short, long, env = "MAKEMKV_KEY")]
    pub makemkv_key: Option<String>,

    /// Minimum title length in seconds (not yet implemented in makemkv.rs but good to have)
    #[arg(long, env = "AUTO_MAKEMKV_MIN_LENGTH", default_value = "120")]
    pub min_length: u64,

    /// Polling interval in seconds
    #[arg(long, env = "AUTO_MAKEMKV_POLL_INTERVAL", default_value = "5")]
    pub poll_interval: u64,

    /// Directory to move successfully ripped discs into. If provided without --failed-dir,
    /// all terminal-state rips (success and failure) are moved here.
    #[arg(long, env = "AUTO_MAKEMKV_COMPLETED_DIR")]
    pub completed_dir: Option<PathBuf>,

    /// Directory to move failed rips into. Only used when the rip fails.
    #[arg(long, env = "AUTO_MAKEMKV_FAILED_DIR")]
    pub failed_dir: Option<PathBuf>,

    /// Pushover application token for notifications.
    #[arg(long, env = "PUSHOVER_APP_TOKEN", requires = "pushover_user_key")]
    pub pushover_app_token: Option<String>,

    /// Pushover user key for notifications.
    #[arg(long, env = "PUSHOVER_USER_KEY", requires = "pushover_app_token")]
    pub pushover_user_key: Option<String>,

    /// Whether to run with pretty progress bars (default: true if stdout is a terminal)
    #[arg(long, short, action = clap::ArgAction::Set, default_value = if std::io::stdout().is_terminal() { "true" } else { "false" })]
    pub interactive: bool,
}
