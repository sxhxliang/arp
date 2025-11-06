pub mod proxy;
pub mod session;

use crate::config::ClientConfig;
use crate::session::SessionManager;
use colored::Colorize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared state for handlers
#[derive(Clone)]
pub struct HandlerState {
    pub config: Arc<ClientConfig>,
    pub session_manager: Arc<Mutex<SessionManager>>,
    pub quiet: bool,
    pub verbose: bool,
    pub raw: bool,
    pub config_path: PathBuf,
    pub upload_dir: String,
}

impl HandlerState {
    pub fn new(config: ClientConfig) -> Self {
        let session_manager = Arc::new(Mutex::new(SessionManager::new(
            config.acp_config.clone().unwrap(),
        )));

        HandlerState {
            config: Arc::new(config),
            session_manager,
            quiet: false,
            verbose: false,
            raw: false,
            config_path: PathBuf::new(),
            upload_dir: String::new(),
        }
    }
    pub fn log(&self, msg: &str) {
        if !self.quiet {
            println!("{} {}", "[arp-acp]".bright_blue(), msg);
        }
    }

    pub fn log_error(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{} {}", "[arp-acp]".bright_red(), msg);
        }
    }

    pub fn pretty_print_message(&self, direction: &str, message: &str) {
        if self.quiet {
            return;
        }

        let dir = if direction.contains("Client → Server") {
            direction.bright_cyan()
        } else {
            direction.bright_green()
        };

        if self.verbose {
            println!(
                "\n{}\n{} | {} {} bytes",
                "═".repeat(80).bright_black(),
                dir,
                message.len().to_string().bright_magenta(),
                "⏰".bright_yellow()
            );
        } else {
            println!("{}", dir);
        }

        if self.raw {
            println!("{}", message);
        } else if let Ok(json) = serde_json::from_str::<Value>(message) {
            println!(
                "{}",
                serde_json::to_string_pretty(&json).unwrap_or_else(|_| message.to_string())
            );
        } else {
            println!("{}", message);
        }

        if self.verbose {
            if !self.raw {
                println!(
                    "{}\n{} {}",
                    "─".repeat(80).bright_black(),
                    "📝 Raw:".bright_blue(),
                    message.dimmed()
                );
            }
            println!("{}", "═".repeat(80).bright_black());
        }
    }
}

pub fn strip_content_length_header(message: &str) -> String {
    if let Some(pos) = message.find("\n\n")
        && message[..pos].starts_with("Content-Length:")
    {
        return message[pos + 2..].to_string();
    }
    message.to_string()
}
