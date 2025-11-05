use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::{env, fs};
use uuid::Uuid;

/// Configuration for the arpc client
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct ClientConfig {
    /// Unique ID for this client instance (Mechine Code).
    #[arg(short, long, default_value_t = default_client_id())]
    pub client_id: String,

    /// Address of the arps server.
    #[arg(short, long, default_value = "proxy.agentx.plus")]
    pub server_addr: String,

    /// Port for the arps control connection.
    #[arg(long, default_value_t = 17001)]
    pub control_port: u16,

    /// Port for the arps proxy connection.
    #[arg(long, default_value_t = 17002)]
    pub proxy_port: u16,

    /// Address of the local service to expose.
    #[arg(long, default_value = "127.0.0.1")]
    pub local_addr: String,

    /// Port of the local service to expose.
    #[arg(long)]
    pub local_port: Option<u16>,

    /// Enable command mode (execute a command instead of TCP proxy)
    #[arg(long, default_value_t = true)]
    pub command_mode: bool,

    /// Command to execute in command mode
    #[arg(long)]
    pub command_path: Option<String>,

    /// Command arguments (comma-separated)
    #[arg(long)]
    pub command_args: Option<String>,

    /// Enable MCP (Model Context Protocol) server
    #[arg(long)]
    pub enable_mcp: bool,

    /// Port for the MCP server
    #[arg(long, default_value_t = 9021)]
    pub mcp_port: u16,

    /// Enable auto-reconnect when connection is lost
    #[arg(long, default_value_t = true)]
    pub auto_reconnect: bool,

    /// Reconnect interval in seconds
    #[arg(long, default_value_t = 5)]
    pub reconnect_interval: u64,

    /// Enable filesystem browsing APIs
    #[arg(long)]
    pub enable_fs: bool,

    /// Path to configuration file
    #[arg(long)]
    pub config: PathBuf,

    /// HTTP port to listen on (for streamable HTTP/SSE)
    #[arg(long, default_value = "3001")]
    pub http_port: u16,

    /// Suppress logging output
    #[arg(short, long, default_value = "false")]
    pub quiet: bool,

    /// Show verbose I/O data (timestamp, length, raw data)
    #[arg(short, long, default_value = "false")]
    pub verbose: bool,

    /// Show raw data without JSON pretty-printing
    #[arg(long, default_value = "false")]
    pub raw: bool,

    /// ACP configuration
    #[clap(skip)]
    pub acp_config: Option<ACPConfig>,
}

fn default_client_id() -> String {
    ClientConfig::generate_machine_code()
}

impl ClientConfig {
    /// Get the server control address
    pub fn control_addr(&self) -> String {
        format!("{}:{}", self.server_addr, self.control_port)
    }

    /// Get the server proxy address
    pub fn proxy_addr(&self) -> String {
        format!("{}:{}", self.server_addr, self.proxy_port)
    }

    /// Get the local service address
    pub fn local_service_addr(&self) -> String {
        match self.local_port {
            Some(port) => format!("{}:{}", self.local_addr, port),
            None => format!("{}:{}", self.local_addr, 80), // Or handle error as appropriate
        }
    }

    /// Ensure a valid client_id is present, generating one if needed.
    pub fn ensure_client_id(&mut self) -> bool {
        if self.client_id.trim().is_empty() {
            self.client_id = Self::generate_machine_code();
            true
        } else {
            false
        }
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), String> {
        // Validate client_id
        if self.client_id.trim().is_empty() {
            return Err("client_id cannot be empty".to_string());
        }

        // Validate ports are different
        if self.control_port == self.proxy_port {
            return Err(format!(
                "control_port ({}) and proxy_port ({}) must be different",
                self.control_port, self.proxy_port
            ));
        }

        // Validate port numbers are valid
        if self.control_port == 0 {
            return Err("control_port cannot be 0".to_string());
        }
        if self.proxy_port == 0 {
            return Err("proxy_port cannot be 0".to_string());
        }
        if self.local_port == Some(0) && !self.command_mode {
            return Err("local_port cannot be 0 when not in command_mode".to_string());
        }

        // Validate server address is not empty
        if self.server_addr.trim().is_empty() {
            return Err("server_addr cannot be empty".to_string());
        }

        // Validate local address is not empty when not in command mode
        if !self.command_mode && self.local_addr.trim().is_empty() {
            return Err("local_addr cannot be empty when not in command_mode".to_string());
        }

        // Validate command_path exists if provided
        if let Some(ref cmd_path) = self.command_path
            && !cmd_path.trim().is_empty()
            && !std::path::Path::new(cmd_path).exists()
        {
            return Err(format!("command_path does not exist: {}", cmd_path));
        }

        // Validate MCP port is different from control and proxy ports
        if self.enable_mcp {
            if self.mcp_port == self.control_port {
                return Err(format!(
                    "mcp_port ({}) cannot be the same as control_port ({})",
                    self.mcp_port, self.control_port
                ));
            }
            if self.mcp_port == self.proxy_port {
                return Err(format!(
                    "mcp_port ({}) cannot be the same as proxy_port ({})",
                    self.mcp_port, self.proxy_port
                ));
            }
        }

        Ok(())
    }

    fn generate_machine_code() -> String {
        let entropy = Self::collect_device_entropy();

        let mut raw = if entropy.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            Uuid::new_v5(&Uuid::NAMESPACE_OID, entropy.as_bytes()).to_string()
        };

        raw.retain(|c| c != '-');
        raw.make_ascii_uppercase();
        raw
    }

    fn collect_device_entropy() -> String {
        let mut parts = Vec::new();

        if let Some(host) = Self::hostname() {
            parts.push(format!("host:{host}"));
        }

        if let Some(machine_id) = Self::machine_id() {
            parts.push(format!("machine_id:{machine_id}"));
        }

        if let Ok(user) = env::var("USER").or_else(|_| env::var("USERNAME"))
            && !user.is_empty()
        {
            parts.push(format!("user:{user}"));
        }

        parts.push(format!("os:{}", env::consts::OS));
        parts.push(format!("arch:{}", env::consts::ARCH));

        #[cfg(target_os = "windows")]
        {
            if let Ok(name) = env::var("COMPUTERNAME")
                && !name.is_empty()
            {
                parts.push(format!("computer:{name}"));
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Some(distro) = Self::linux_os_release() {
                parts.push(format!("distro:{distro}"));
            }
        }

        parts.join("|")
    }

    fn hostname() -> Option<String> {
        hostname::get().ok()?.into_string().ok()
    }

    fn machine_id() -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            const PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];
            for path in PATHS {
                if let Some(value) = Self::read_trimmed(path) {
                    return Some(value);
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            if let Some(value) = Self::read_trimmed("/etc/hostid") {
                return Some(value);
            }
        }

        None
    }

    #[cfg(target_os = "linux")]
    fn linux_os_release() -> Option<String> {
        use std::io::Read;

        let mut file = fs::File::open("/etc/os-release").ok()?;
        let mut content = String::new();
        file.read_to_string(&mut content).ok()?;

        for line in content.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                return Some(value.trim_matches('"').to_string());
            }
        }

        None
    }

    #[cfg(not(target_os = "linux"))]
    fn linux_os_release() -> Option<String> {
        None
    }

    fn read_trimmed(path: &str) -> Option<String> {
        let content = fs::read_to_string(path).ok()?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

/// Configuration for an agent server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Root configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ACPConfig {
    pub agent_servers: HashMap<String, AgentConfig>,
    #[serde(default = "default_upload_dir")]
    pub upload_dir: String,
}

fn default_upload_dir() -> String {
    ".".to_string()
}

impl ACPConfig {
    pub fn load(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: ACPConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}
