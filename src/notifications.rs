use log::{info, warn};
use std::error::Error;
use std::path::Path;
use std::sync::OnceLock;

static PUSHOVER: OnceLock<Pushover> = OnceLock::new();

pub fn init_pushover(app_token: &str, user_key: &str) {
    PUSHOVER.get_or_init(|| Pushover::new(app_token, user_key));
}

pub fn pushover() -> Option<&'static Pushover> {
    PUSHOVER.get()
}

pub struct Pushover {
    app_token: String,
    user_key: String,
    client: reqwest::Client,
}

impl Pushover {
    pub fn new(app_token: &str, user_key: &str) -> Self {
        Self {
            app_token: app_token.to_string(),
            user_key: user_key.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn send_pushover(&self, title: &str, message: &str) {
        use std::collections::HashMap;

        let mut params = HashMap::new();
        params.insert("token", self.app_token.clone());
        params.insert("user", self.user_key.clone());
        params.insert("title", title.to_string());
        params.insert("message", message.to_string());

        match self
            .client
            .post("https://api.pushover.net/1/messages.json")
            .form(&params)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!("Pushover notification sent: {}", title);
            }
            Ok(resp) => {
                warn!(
                    "Pushover notification failed (HTTP {}): {}",
                    resp.status(),
                    title
                );
            }
            Err(e) => {
                warn!(
                    "Pushover notification error: {}: {}",
                    e,
                    e.source().map(|s| s.to_string()).unwrap_or_default()
                );
            }
        }
    }

    pub async fn notify_rip_started(
        &self,
        drive_index: usize,
        disc_label: &str,
        output_dir: &Path,
    ) {
        self.send_pushover(
            "Disc rip started",
            &format!(
                "Started ripping '{}' (drive {}) to {}",
                disc_label,
                drive_index,
                output_dir.display()
            ),
        )
        .await;
    }

    pub async fn notify_rip_completed(&self, drive_index: usize, disc_label: &str) {
        self.send_pushover(
            "Disc rip complete",
            &format!(
                "Successfully ripped '{}' (drive {}).",
                disc_label, drive_index
            ),
        )
        .await;
    }

    pub async fn notify_rip_failed(&self, drive_index: usize, disc_label: &str) {
        self.send_pushover(
            "Disc rip failed",
            &format!("Failed to rip '{}' (drive {}).", disc_label, drive_index),
        )
        .await;
    }
}
