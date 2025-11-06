use bytes::Bytes;
use clap::Parser;
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, info};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::BroadcastStream;
use tokio_tungstenite::tungstenite::Message;

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
pub struct Config {
    pub agent_servers: HashMap<String, AgentConfig>,
}

impl Config {
    pub fn load(path: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_json::from_str(&content)?;
        Ok(config)
    }
}

/// Represents a single agent session
struct Session {
    session_id: String,
    agent_name: String,
    child: Child,
    stdin: ChildStdin,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    real_session_id: Arc<Mutex<Option<String>>>,
    created_at: String,
    updated_at: Arc<Mutex<String>>,
    cwd: String,
    title: Option<String>,
    // Broadcast channel for sending messages to multiple subscribers (WebSocket, HTTP/SSE)
    output_tx: broadcast::Sender<String>,
}

/// Manages multiple agent sessions
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    config: Config,
}

impl SessionManager {
    pub fn new(config: Config) -> Self {
        Self {
            sessions: HashMap::new(),
            config,
        }
    }

    /// Create a new session and spawn the agent process
    async fn create_session(
        &mut self,
        agent_name: &str,
        cwd: String,
        state: Arc<ServerState>,
        session_manager: Arc<Mutex<SessionManager>>,
    ) -> Result<String, String> {
        // Get agent configuration
        let agent_config = self.config.agent_servers.get(agent_name)
            .ok_or_else(|| format!("Agent '{}' not found in configuration", agent_name))?
            .clone();
        info!("Creating session for agent '{:?}'", agent_config);
        // Spawn the child process
        let mut cmd = Command::new(&agent_config.command);
        cmd.args(&agent_config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Add environment variables
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

        // We'll store a temporary session ID for now
        // The actual session ID will be updated after we get the response from the child
        let temp_session_id = format!("temp-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis());

        // Shared session ID that will be updated when we get the real one
        let real_session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Create broadcast channel for output
        let (output_tx, _output_rx) = broadcast::channel::<String>(1000);

        // Task to read from child stdout and broadcast to all subscribers
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

                    // Check if this is a session/new response and extract the sessionId
                    if let Ok(json) = serde_json::from_str::<Value>(&content) {
                        if let Some(result) = json.get("result") {
                            if let Some(session_id) = result.get("sessionId").and_then(|s| s.as_str()) {
                                // This is the session/new response
                                // state.log(&format!("Received sessionId from child: {}", session_id));

                                // Update the session ID in the manager
                                let mut manager = session_manager.lock().await;
                                manager.update_session_id(&temp_session_id, session_id).await;

                                // Update our local reference
                                *real_session_id.lock().await = Some(session_id.to_string());
                            }
                        }
                    }

                    // Add _meta.sessionId to the response if we have a real session ID
                    if let Some(session_id) = real_session_id.lock().await.as_ref() {
                        content = add_session_id_to_message(&content, session_id);
                    }

                    // state.pretty_print_message("[Server → Client]", &content);

                    // Broadcast to all subscribers (WebSocket, HTTP/SSE)
                    let _ = output_tx.send(content);
                }
            })
        };

        // Task to read from child stderr and log
        let stderr_task = {
            let state = state.clone();
            tokio::spawn(async move {
                let mut lines = stderr_reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // state.log_error(&format!("Child stderr: {}", line));
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
    async fn update_session_id(&mut self, old_id: &str, new_id: &str) {
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

    /// Get list of available agents from config
    pub fn list_agents(&self) -> Vec<serde_json::Value> {
        self.config.agent_servers.iter().map(|(name, agent_config)| {
            serde_json::json!({
                "name": name,
                "command": agent_config.command,
                "args": agent_config.args,
                "env": agent_config.env
            })
        }).collect()
    }

    fn get_session_stdin(&mut self, session_id: &str) -> Option<&mut ChildStdin> {
        self.sessions.get_mut(session_id).map(|s| &mut s.stdin)
    }

    fn subscribe_to_session(&self, session_id: &str) -> Option<broadcast::Receiver<String>> {
        self.sessions.get(session_id).map(|s| s.output_tx.subscribe())
    }

    async fn close_session(&mut self, session_id: &str, state: Arc<ServerState>) {
        if let Some(mut session) = self.sessions.remove(session_id) {
            // state.log(&format!("Closing session: {}", session_id));
            drop(session.stdin);
            let timeout = tokio::time::Duration::from_secs(5);
            let _ = tokio::time::timeout(timeout, session.stdout_task).await;
            let _ = tokio::time::timeout(timeout, session.stderr_task).await;
            let _ = session.child.kill().await;
            let _ = session.child.wait().await;
        }
    }

    /// Handle /session/new request - returns the response message or error
    /// This is the core logic extracted from the HTTP handler for reuse in TCP handler
    pub async fn handle_new_session_request(
        &mut self,
        body:&[u8],
        state: Arc<ServerState>,
        session_manager: Arc<Mutex<SessionManager>>,
    ) -> Result<String, String> {
        let json: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let params = json.get("params").unwrap();
        info!("New session request: {:?}", body);
        // Extract agent name and cwd
        let Some(agent_name) = params["_meta"]["agentName"].as_str() else {
            return Err("Missing _meta.agentName".to_string());
        };
        let cwd = params["cwd"].as_str().unwrap_or(".").to_string();
        info!("Agent name: {}", agent_name);
        // Create session
        let temp_session_id = self.create_session(agent_name, cwd, state.clone(), session_manager.clone()).await
            .map_err(|e| format!("Failed to create session: {}", e))?;

        // Get stdin and subscribe
        let Some(stdin) = self.get_session_stdin(&temp_session_id) else {
            return Err("Failed to get session stdin".to_string());
        };
        info!("Created session {}", temp_session_id);
        // Send request to child

        if let Err(e) = stdin.write_all(body).await {
            // state.log_error(&format!("Failed to write to child stdin: {}", e));
           error!("Failed to write to child stdin: {}", e);
        }
        let _ = stdin.flush().await;
        info!("Wrote to stdin");
        // Subscribe and wait for response
        let Some(mut receiver) = self.subscribe_to_session(&temp_session_id) else {
            return Err("Failed to subscribe to session".to_string());
        };

        match tokio::time::timeout(tokio::time::Duration::from_secs(30), receiver.recv()).await {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(e)) => Err(format!("Broadcast error: {}", e)),
            Err(_) => Err("Timeout waiting for session/new response".to_string()),
        }
    }

    /// Handle /session/prompt request - returns a receiver for streaming or error
    /// This is the core logic extracted from the HTTP handler for reuse in TCP handler
    pub async fn handle_prompt_request(
        &mut self,
        body: &Value,
    ) -> Result<broadcast::Receiver<String>, String> {
        // Extract session ID
        let Some(session_id) = body["_meta"]["sessionId"].as_str()
            .or_else(|| body["sessionId"].as_str()) else {
            return Err("Missing _meta.sessionId or sessionId".to_string());
        };

        // Subscribe first
        let Some(receiver) = self.subscribe_to_session(session_id) else {
            return Err("Session not found".to_string());
        };

        // Prepare params
        let mut params = body.clone();
        if let Some(obj) = params.as_object_mut() {
            obj.insert("sessionId".to_string(), Value::String(session_id.to_string()));
        }

        // Create JSON-RPC request
        let jsonrpc_request = serde_json::json!({
            "id": 1,
            "jsonrpc": "2.0",
            "method": "session/prompt",
            "params": params
        });

        // Get stdin and send request
        let Some(stdin) = self.get_session_stdin(session_id) else {
            return Err("Session not found".to_string());
        };

        let request_str = format!("{}\n", jsonrpc_request.to_string());
        stdin.write_all(request_str.as_bytes()).await
            .map_err(|e| format!("Failed to write to stdin: {}", e))?;
        stdin.flush().await
            .map_err(|e| format!("Failed to flush stdin: {}", e))?;

        Ok(receiver)
    }
}

/// Bridge stdio processes to WebSocket connections
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: PathBuf,

    /// WebSocket port to listen on
    #[arg(short, long, default_value = "3000")]
    port: u16,

    /// HTTP port to listen on (for streamable HTTP/SSE)
    #[arg(long, default_value = "3001")]
    http_port: u16,

    /// Suppress logging output
    #[arg(short, long, default_value = "false")]
    quiet: bool,

    /// Show verbose I/O data (timestamp, length, raw data)
    #[arg(short, long, default_value = "false")]
    verbose: bool,

    /// Show raw data without JSON pretty-printing
    #[arg(long, default_value = "false")]
    raw: bool,
}

pub struct ServerState {
    pub quiet: bool,
    pub verbose: bool,
    pub raw: bool,
}


fn strip_content_length_header(message: &str) -> String {
    if let Some(pos) = message.find("\n\n") {
        if message[..pos].starts_with("Content-Length:") {
            return message[pos + 2..].to_string();
        }
    }
    message.to_string()
}

fn extract_agent_name(json: &Value) -> Option<String> {
    json["params"]["_meta"]["agentName"].as_str().map(String::from)
}

fn extract_session_id(json: &Value) -> Option<String> {
    json["params"]["_meta"]["sessionId"].as_str()
        .or_else(|| json["params"]["sessionId"].as_str())
        .or_else(|| json["result"]["_meta"]["sessionId"].as_str())
        .map(String::from)
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

fn create_error_response(id: Option<i64>, code: i32, message: &str) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

fn json_response(status: StatusCode, json: Value) -> Response<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .body(Full::new(Bytes::from(json.to_string())).boxed())
        .unwrap()
}

fn error_response(status: StatusCode, message: &str) -> Response<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    json_response(status, serde_json::json!({"error": message}))
}

/// Handle HTTP requests
async fn handle_json_rpc_request(
    req: Request<hyper::body::Incoming>,
    session_manager: Arc<Mutex<SessionManager>>,
    state: Arc<ServerState>,
) -> Result<Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    match (method, path.as_str()) {
        // GET /session/list - List all sessions
        (Method::GET, "/session/list") => {
            let sessions = session_manager.lock().await.list_sessions();
            let response_json = serde_json::json!({
                "sessions": sessions
            });
            let body = serde_json::to_string(&response_json).unwrap();

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .header("Access-Control-Allow-Origin", "*")
                .body(Full::new(Bytes::from(body)).boxed())
                .unwrap())
        }

        // POST /session/new - Create a new session
        (Method::POST, "/session/new") => {
            let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
            let body_str = String::from_utf8_lossy(&body_bytes);
            let body_one_line = if let Ok(json) = serde_json::from_str::<Value>(&body_str) {
                serde_json::to_string(&json).unwrap_or_else(|_| body_str.to_string())
            } else {
                body_str.to_string()
            };

            // state.pretty_print_message("[HTTP Client → Server]", &body_one_line);

            let json: Value = match serde_json::from_str(&body_one_line) {
                Ok(json) => json,
                Err(e) => return Ok(error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {}", e))),
            };

            let Some(agent_name) = json["_meta"]["agentName"].as_str() else {
                return Ok(error_response(StatusCode::BAD_REQUEST, "Missing _meta.agentName"));
            };

            let cwd = json["cwd"].as_str().unwrap_or(".").to_string();

            let jsonrpc_request = serde_json::json!({"id": 0, "jsonrpc": "2.0", "method": "session/new", "params": json});

            let mut manager = session_manager.lock().await;
            let temp_session_id = match manager.create_session(&agent_name, cwd, state.clone(), session_manager.clone()).await {
                Ok(id) => id,
                Err(e) => return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, &e)),
            };

            // state.log(&format!("Created session for agent: {}", agent_name));

            let Some(stdin) = manager.get_session_stdin(&temp_session_id) else {
                return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to get session stdin"));
            };

            let request_str = format!("{}\n", jsonrpc_request.to_string());
            // state.log(&format!("Sending to child stdin: {}", request_str.trim()));

            if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                // state.log_error(&format!("Failed to write to child stdin: {}", e));
                return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to communicate with agent"));
            }
            let _ = stdin.flush().await;

            let Some(mut receiver) = manager.subscribe_to_session(&temp_session_id) else {
                return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to subscribe to session"));
            };
            drop(manager);

            match tokio::time::timeout(tokio::time::Duration::from_secs(30), receiver.recv()).await {
                Ok(Ok(message)) => {
                    // state.pretty_print_message("[Server → HTTP Client]", &message);
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Full::new(Bytes::from(message)).boxed())
                        .unwrap())
                }
                Ok(Err(e)) => Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("Broadcast error: {}", e))),
                Err(_) => Ok(error_response(StatusCode::GATEWAY_TIMEOUT, "Timeout waiting for session/new response")),
            }
        }

        // POST /session/prompt - Send prompt and stream response via SSE
        (Method::POST, "/session/prompt") => {
            let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
            let body_str = String::from_utf8_lossy(&body_bytes);
            let body_one_line = if let Ok(json) = serde_json::from_str::<Value>(&body_str) {
                serde_json::to_string(&json).unwrap_or_else(|_| body_str.to_string())
            } else {
                body_str.to_string()
            };

            // state.pretty_print_message("[HTTP Client → Server]", &body_one_line);

            let json: Value = match serde_json::from_str(&body_str) {
                Ok(json) => json,
                Err(e) => return Ok(error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {}", e))),
            };

            let Some(session_id) = json["_meta"]["sessionId"].as_str()
                .or_else(|| json["sessionId"].as_str()) else {
                return Ok(error_response(StatusCode::BAD_REQUEST, "Missing _meta.sessionId or sessionId"));
            };

            let receiver = {
                let manager = session_manager.lock().await;
                manager.subscribe_to_session(session_id)
            };

            let Some(receiver) = receiver else {
                return Ok(error_response(StatusCode::NOT_FOUND, "Session not found"));
            };

            let mut params = json.clone();
            if let Some(obj) = params.as_object_mut() {
                obj.insert("sessionId".to_string(), Value::String(session_id.to_string()));
            }

            let jsonrpc_request = serde_json::json!({"id": 1, "jsonrpc": "2.0", "method": "session/prompt", "params": params});

            {
                let mut manager = session_manager.lock().await;
                let Some(stdin) = manager.get_session_stdin(session_id) else {
                    return Ok(error_response(StatusCode::NOT_FOUND, "Session not found"));
                };

                let request_str = format!("{}\n", jsonrpc_request.to_string());
                // state.log(&format!("Sending to child stdin: {}", request_str.trim()));

                if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                    // state.log_error(&format!("Failed to write to child stdin: {}", e));
                    return Ok(error_response(StatusCode::INTERNAL_SERVER_ERROR, "Failed to communicate with session"));
                }
                let _ = stdin.flush().await;
            }

            use std::future::ready;
            use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

            let sse_stream = BroadcastStream::new(receiver)
                .flat_map(|result: Result<String, BroadcastStreamRecvError>| {
                    match result {
                        Ok(message) => {
                            let is_end = serde_json::from_str::<Value>(&message)
                                .ok()
                                .and_then(|json| json.get("result")?.get("stopReason")?.as_str().map(|s| s.to_string()))
                                .map(|s| s == "end_turn")
                                .unwrap_or(false);

                            let frame = Ok(Frame::data(Bytes::from(format!("data: {}\n\n", message))));
                            if is_end {
                                tokio_stream::iter(vec![Some(frame), None])
                            } else {
                                tokio_stream::iter(vec![Some(frame)])
                            }
                        }
                        Err(e) => tokio_stream::iter(vec![Some(Ok(Frame::data(Bytes::from(format!("data: {{\"error\": \"{}\"}}\n\n", e)))))]),
                    }
                })
                .take_while(|opt| ready(opt.is_some()))
                .map(|opt| opt.unwrap());

            let body = BodyExt::boxed(StreamBody::new(sse_stream));

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .header("Access-Control-Allow-Origin", "*")
                .body(body)
                .unwrap())
        }

        (Method::OPTIONS, _) => {
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                .header("Access-Control-Allow-Headers", "Content-Type")
                .body(Full::new(Bytes::new()).boxed())
                .unwrap())
        }

        _ => Ok(error_response(StatusCode::NOT_FOUND, "Not found"))
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Load configuration file
    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Failed to load configuration file: {}", e);
            std::process::exit(1);
        }
    };

    let http_addr: SocketAddr = format!("0.0.0.0:{}", args.http_port).parse()?;

    let state = Arc::new(ServerState {
        quiet: args.quiet,
        verbose: args.verbose,
        raw: args.raw,
    });

    // Create global session manager
    let session_manager = Arc::new(Mutex::new(SessionManager::new(config.clone())));


    // HTTP server task
    let http_session_manager = session_manager.clone();
    let http_state = state.clone();
    
    let listener = match tokio::net::TcpListener::bind(&http_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind HTTP server: {}", e);
            return Err(Box::new(e));
        }
    };

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                // http_state.log_error(&format!("Failed to accept HTTP connection: {}", e));
                continue;
            }
        };

        let io = TokioIo::new(stream);
        let session_manager = http_session_manager.clone();
        let state = http_state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let session_manager = session_manager.clone();
                let state = state.clone();
                handle_json_rpc_request(req, session_manager, state)
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                eprintln!("HTTP connection error: {}", e);
            }
        });
    }


    Ok(())
}
