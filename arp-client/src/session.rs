use crate::config::{AgentConfig, ACPConfig};
use crate::handlers::{strip_content_length_header, HandlerState};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;

use tokio::sync::{Mutex, RwLock, broadcast};



/// Executor type for command execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutorKind {
    ACPAgent,
    Command,
}


/// Represents a single agent session
pub struct Session {
    pub session_id: String,
    pub agent_name: String,
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout_task: JoinHandle<()>,
    pub stderr_task: JoinHandle<()>,
    pub real_session_id: Arc<Mutex<Option<String>>>,
    pub created_at: String,
    pub updated_at: Arc<Mutex<String>>,
    pub cwd: String,
    pub title: Option<String>,
    pub output_tx: broadcast::Sender<String>,
}

/// Manages multiple agent sessions

pub struct SessionManager {
    pub sessions: HashMap<String, Session>,
    agent_session_map: Arc<Mutex<HashMap<(ExecutorKind, String), String>>>,
    config: ACPConfig,
}

impl SessionManager {
    pub fn new(config: ACPConfig) -> Self {
        Self {
            sessions: HashMap::new(),
            agent_session_map: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Create a new session and spawn the agent process
    pub async fn create_session(
        &mut self,
        agent_name: &str,
        cwd: String,
        state: Arc<HandlerState>,
        session_manager: Arc<Mutex<SessionManager>>,
    ) -> Result<String, String> {
        let agent_config = self.config.agent_servers.get(agent_name)
            .ok_or_else(|| format!("Agent '{}' not found in configuration", agent_name))?
            .clone();

        let mut cmd = if cfg!(target_os = "windows") {
            let mut shell_cmd = Command::new("cmd");
            let mut full_args = vec!["/C".to_string(), agent_config.command.clone()];
            full_args.extend(agent_config.args.iter().cloned());
            shell_cmd.args(&full_args);
            shell_cmd
        } else {
            let mut cmd = Command::new(&agent_config.command);
            cmd.args(&agent_config.args);
            cmd
        };

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (key, value) in &agent_config.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn()
            .map_err(|e| format!("Failed to spawn child process: {}", e))?;

        let stdin = child.stdin.take()
            .ok_or_else(|| "Failed to open stdin".to_string())?;
        let stdout = child.stdout.take()
            .ok_or_else(|| "Failed to open stdout".to_string())?;
        let stderr = child.stderr.take()
            .ok_or_else(|| "Failed to open stderr".to_string())?;

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        let temp_session_id = format!("temp-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis());

        let real_session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (output_tx, _output_rx) = broadcast::channel::<String>(1000);

        let stdout_task = {
            let state = state.clone();
            let real_session_id = real_session_id.clone();
            let temp_session_id = temp_session_id.clone();
            let session_manager = session_manager.clone();
            let output_tx = output_tx.clone();

            tokio::spawn(async move {
                let mut lines = stdout_reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut content = strip_content_length_header(&line);

                    if let Ok(json) = serde_json::from_str::<Value>(&content) {
                        if let Some(result) = json.get("result") {
                            if let Some(session_id) = result.get("sessionId").and_then(|s| s.as_str()) {
                                state.log(&format!("Received sessionId from child: {}", session_id));

                                let mut manager = session_manager.lock().await;
                                manager.update_session_id(&temp_session_id, session_id).await;

                                *real_session_id.lock().await = Some(session_id.to_string());
                            }
                        }
                    }

                    if let Some(session_id) = real_session_id.lock().await.as_ref() {
                        content = add_session_id_to_message(&content, session_id);
                    }

                    state.pretty_print_message("[Server → Client]", &content);
                    let _ = output_tx.send(content);
                }
            })
        };

        let stderr_task = {
            let state = state.clone();
            tokio::spawn(async move {
                let mut lines = stderr_reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    state.log_error(&format!("Child stderr: {}", line));
                }
            })
        };

        let created_at = chrono::Utc::now().to_rfc3339();
        let updated_at = Arc::new(Mutex::new(created_at.clone()));

        let session = Session {
            session_id: temp_session_id.clone(),
            agent_name: agent_name.to_string(),
            child,
            stdin,
            stdout_task,
            stderr_task,
            real_session_id: real_session_id.clone(),
            created_at,
            updated_at,
            cwd,
            title: None,
            output_tx,
        };

        self.sessions.insert(temp_session_id.clone(), session);

        Ok(temp_session_id)
    }

    /// Update the session ID after receiving response from child
    pub async fn update_session_id(&mut self, old_id: &str, new_id: &str) {
        if let Some(mut session) = self.sessions.remove(old_id) {
            session.session_id = new_id.to_string();
            *session.real_session_id.lock().await = Some(new_id.to_string());
            *session.updated_at.lock().await = chrono::Utc::now().to_rfc3339();
            self.sessions.insert(new_id.to_string(), session);
        }
    }

    pub fn list_sessions(&self) -> Vec<serde_json::Value> {
        self.sessions.values().map(|s| {
            let updated = s.updated_at.try_lock().map(|u| u.clone()).unwrap_or_else(|_| s.created_at.clone());
            serde_json::json!({
                "sessionId": s.session_id,
                "createdAt": s.created_at,
                "updatedAt": updated,
                "cwd": s.cwd,
                "title": s.title,
                "_meta": {"agentName": s.agent_name}
            })
        }).collect()
    }

    pub fn get_session_stdin(&mut self, session_id: &str) -> Option<&mut ChildStdin> {
        self.sessions.get_mut(session_id).map(|s| &mut s.stdin)
    }

    pub fn subscribe_to_session(&self, session_id: &str) -> Option<broadcast::Receiver<String>> {
        self.sessions.get(session_id).map(|s| s.output_tx.subscribe())
    }

    pub async fn close_session(&mut self, session_id: &str, state: Arc<HandlerState>) {
        if let Some(mut session) = self.sessions.remove(session_id) {
            state.log(&format!("Closing session: {}", session_id));
            drop(session.stdin);
            let timeout = tokio::time::Duration::from_secs(5);
            let _ = tokio::time::timeout(timeout, session.stdout_task).await;
            let _ = tokio::time::timeout(timeout, session.stderr_task).await;
            let _ = session.child.kill().await;
            match session.child.wait().await {
                Ok(status) => state.log(&format!("Session {} exited with code {:?}", session_id, status.code())),
                Err(e) => state.log_error(&format!("Failed to wait for child process: {}", e)),
            }
        }
    }

    /// List all configured agents
    pub fn list_agents(&self) -> Vec<(String, AgentConfig)> {
        self.config.agent_servers.iter()
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect()
    }

    /// Get a specific agent configuration
    pub fn get_agent(&self, name: &str) -> Option<AgentConfig> {
        self.config.agent_servers.get(name).cloned()
    }

    /// Add a new agent configuration
    pub fn add_agent(&mut self, name: String, config: AgentConfig) -> Result<(), String> {
        if self.config.agent_servers.contains_key(&name) {
            return Err(format!("Agent '{}' already exists", name));
        }
        self.config.agent_servers.insert(name, config);
        Ok(())
    }

    /// Update an existing agent configuration
    pub fn update_agent(&mut self, name: &str, config: AgentConfig) -> Result<(), String> {
        if !self.config.agent_servers.contains_key(name) {
            return Err(format!("Agent '{}' not found", name));
        }
        self.config.agent_servers.insert(name.to_string(), config);
        Ok(())
    }

    /// Delete an agent configuration
    pub fn delete_agent(&mut self, name: &str) -> Result<(), String> {
        if !self.config.agent_servers.contains_key(name) {
            return Err(format!("Agent '{}' not found", name));
        }
        self.config.agent_servers.remove(name);
        Ok(())
    }

    /// Save configuration to file
    pub fn save_config(&self, path: &PathBuf) -> Result<(), String> {
        self.config.save(path).map_err(|e| format!("Failed to save config: {}", e))
    }
}

fn add_session_id_to_message(message: &str, session_id: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<Value>(message) else {
        return message.to_string();
    };

    let meta = serde_json::json!({"sessionId": session_id});

    if let Some(obj) = json["result"].as_object_mut() {
        obj.entry("_meta").or_insert(meta);
    } else if let Some(obj) = json["params"].as_object_mut() {
        if obj.contains_key("sessionId") {
            obj.entry("_meta").or_insert(meta);
        }
    }

    serde_json::to_string(&json).unwrap_or_else(|_| message.to_string())
}
