use anyhow::{anyhow, Result};
use clap::Parser;
use common::{read_command, write_command, join_streams, Command, http::HttpRequest};
use tokio::net::TcpStream;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, AsyncReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tracing::{info, error, warn, Level};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::path::PathBuf;

mod acp;
mod jsonrpc;
use acp::{Config, SessionManager, ServerState};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Unique ID for this client instance.
    #[arg(short, long)]
    client_id: String,

    /// Address of the frps server.
    #[arg(short, long, default_value = "101.201.174.95")]
    server_addr: String,

    /// Port for the frps control connection.
    #[arg(long, default_value_t = 17001)]
    control_port: u16,

    /// Port for the frps proxy connection.
    #[arg(long, default_value_t = 17002)]
    proxy_port: u16,

    /// Address of the local service to expose.
    #[arg(long, default_value = "127.0.0.1")]
    local_addr: String,

    /// Port of the local service to expose.
    #[arg(long, default_value_t = 3000)]
    local_port: u16,

    /// Enable command mode (execute a command instead of TCP proxy)
    #[arg(long)]
    command_mode: bool,

    /// Path to configuration file (required in command mode)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Command to execute in command mode (deprecated - use config file)
    #[arg(long)]
    command_path: Option<String>,

    /// Command arguments (comma-separated) (deprecated - use config file)
    #[arg(long)]
    command_args: Option<String>,
}

/// Parsed HTTP request structure
#[derive(Debug)]
struct ParsedRequest {
    method: String,
    path: String,
    version: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// Parse an HTTP request from a TCP stream
async fn parse_http_request(stream: &mut TcpStream) -> Result<ParsedRequest> {
    let mut buffer = Vec::new();
    let mut temp_buf = [0u8; 8192];

    // Read headers until we find \r\n\r\n
    let mut header_end = 0;
    loop {
        let n = stream.read(&mut temp_buf).await?;
        if n == 0 {
            return Err(anyhow!("Connection closed while reading headers"));
        }

        buffer.extend_from_slice(&temp_buf[..n]);

        // Look for \r\n\r\n
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }

        // Prevent header bomb
        if buffer.len() > 16384 {
            return Err(anyhow!("Headers too large"));
        }
    }

    // Parse request line and headers
    let header_str = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_str.lines();

    // Parse request line: METHOD PATH HTTP/VERSION
    let request_line = lines.next()
        .ok_or_else(|| anyhow!("Missing request line"))?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(anyhow!("Invalid request line"));
    }

    let method = parts[0].to_string();
    let path = parts[1].to_string();
    let version = parts[2].to_string();

    // Parse headers
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_lowercase();
            let value = line[colon_pos + 1..].trim().to_string();
            headers.insert(key, value);
        }
    }

    // Read body if Content-Length is present
    let content_length: usize = headers.get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body = Vec::new();

    // Check if we already read some body data
    if header_end < buffer.len() {
        body.extend_from_slice(&buffer[header_end..]);
    }

    // Read remaining body
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let to_read = remaining.min(temp_buf.len());
        let n = stream.read(&mut temp_buf[..to_read]).await?;
        if n == 0 {
            return Err(anyhow!("Connection closed while reading body"));
        }
        body.extend_from_slice(&temp_buf[..n]);
    }

    Ok(ParsedRequest {
        method,
        path,
        version,
        headers,
        body,
    })
}

/// Write a standard JSON HTTP response to TCP stream
async fn write_json_response(
    stream: &mut TcpStream,
    status_code: u16,
    status_text: &str,
    json_body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: keep-alive\r\n\
         \r\n\
         {}",
        status_code,
        status_text,
        json_body.len(),
        json_body
    );

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Write error response
async fn write_error_response(
    stream: &mut TcpStream,
    status_code: u16,
    status_text: &str,
    error_message: &str,
) -> Result<()> {
    let json = json!({"error": error_message}).to_string();
    write_json_response(stream, status_code, status_text, &json).await
}

/// Write SSE stream response with chunked encoding
async fn write_sse_stream(
    stream: &mut TcpStream,
    mut receiver: tokio::sync::broadcast::Receiver<String>,
) -> Result<()> {
    use serde_json::Value;

    // Write HTTP response headers for SSE
    let headers = "HTTP/1.1 200 OK\r\n\
                   Content-Type: text/event-stream\r\n\
                   Cache-Control: no-cache\r\n\
                   Connection: keep-alive\r\n\
                   Transfer-Encoding: chunked\r\n\
                   Access-Control-Allow-Origin: *\r\n\
                   \r\n";

    stream.write_all(headers.as_bytes()).await?;
    stream.flush().await?;

    // Stream messages
    loop {
        match receiver.recv().await {
            Ok(message) => {
                // Format as SSE: data: {json}\n\n
                let sse_data = format!("data: {}\n\n", message);

                // Write as chunked
                let chunk = format!("{:X}\r\n{}\r\n", sse_data.len(), sse_data);
                stream.write_all(chunk.as_bytes()).await?;
                stream.flush().await?;

                // Check if this is the end
                if let Ok(json) = serde_json::from_str::<Value>(&message) {
                    if let Some(stop_reason) = json.get("result")
                        .and_then(|r| r.get("stopReason"))
                        .and_then(|s| s.as_str())
                    {
                        if stop_reason == "end_turn" {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                // Send error and close
                let error_msg = format!("data: {{\"error\": \"{}\"}}\n\n", e);
                let chunk = format!("{:X}\r\n{}\r\n", error_msg.len(), error_msg);
                let _ = stream.write_all(chunk.as_bytes()).await;
                break;
            }
        }
    }

    // Send final chunk (0 length) to close stream
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    info!("Starting frpc with client_id: {}", args.client_id);
    info!("Server address: {}:{}", args.server_addr, args.control_port);

    // Load config and create SessionManager if in command mode
    let session_manager: Option<Arc<Mutex<SessionManager>>> = if args.command_mode {
        let config_path = args.config.as_ref()
            .ok_or_else(|| anyhow!("Config file path required in command mode"))?;

        let config = Config::load(config_path)
            .map_err(|e| anyhow!("Failed to load config: {}", e))?;

        info!("Loaded configuration with {} agent servers", config.agent_servers.len());
        Some(Arc::new(Mutex::new(SessionManager::new(config))))
    } else {
        info!("Local service: {}:{}", args.local_addr, args.local_port);
        None
    };

    let server_state = Arc::new(ServerState {
        quiet: false,
        verbose: false,
        raw: false,
    });

    info!("Connected to control port.");

    let control_stream = TcpStream::connect(format!("{}:{}", args.server_addr, args.control_port)).await?;
    info!("Connected to control port.");

    let (mut reader, mut writer) = tokio::io::split(control_stream);

    // Register the client
    let register_cmd = Command::Register { client_id: args.client_id.clone() };
    write_command(&mut writer, &register_cmd).await?;

    // Wait for registration result
    match read_command(&mut reader).await? {
        Command::RegisterResult { success, error } => {
            if success {
                info!("Successfully registered with the server.");
            } else {
                error!("Registration failed: {}", error.unwrap_or_default());
                return Err(anyhow!("Registration failed"));
            }
        }
        _ => {
            return Err(anyhow!("Received unexpected command after registration attempt."));
        }
    }

    // Main loop to listen for commands from the server
    loop {
        match read_command(&mut reader).await {
            Ok(Command::RequestNewProxyConn { proxy_conn_id }) => {
                info!("Received request for new proxy connection: {}", proxy_conn_id);
                let args_clone = args.clone();
                let session_manager_clone = session_manager.clone();
                let server_state_clone = server_state.clone();
                tokio::spawn(async move {
                    if let Err(e) = create_proxy_connection(
                        args_clone,
                        proxy_conn_id,
                        session_manager_clone,
                        server_state_clone,
                    ).await {
                        error!("Failed to create proxy connection: {}", e);
                    }
                });
            }
            Ok(cmd) => {
                warn!("Received unexpected command: {:?}", cmd);
            }
            Err(ref e) if e.downcast_ref::<io::Error>().map_or(false, |io_err| io_err.kind() == io::ErrorKind::UnexpectedEof) => {
                error!("Control connection closed by server. Shutting down.");
                break;
            }
            Err(e) => {
                error!("Error reading from control connection: {}. Shutting down.", e);
                break;
            }
        }
    }

    Ok(())
}

async fn create_proxy_connection(
    args: Args,
    proxy_conn_id: String,
    session_manager: Option<Arc<Mutex<SessionManager>>>,
    server_state: Arc<ServerState>,
) -> Result<()> {
    let mut proxy_stream = TcpStream::connect(format!("{}:{}", args.server_addr, args.proxy_port)).await?;
    info!("('{}') Connected to proxy port.", proxy_conn_id);

    let notify_cmd = Command::NewProxyConn { proxy_conn_id: proxy_conn_id.clone(), client_id: args.client_id.clone() };
    write_command(&mut proxy_stream, &notify_cmd).await?;
    info!("('{}') Sent new proxy connection notification.", proxy_conn_id);

    if args.command_mode {
        // Command mode: handle HTTP/JSON-RPC requests
        info!("('{}') Running in command mode", proxy_conn_id);
        let session_manager = session_manager
            .ok_or_else(|| anyhow!("SessionManager not initialized for command mode"))?;
        execute_command_and_stream(proxy_stream, &proxy_conn_id, session_manager, server_state).await?;
    } else {
        // Normal TCP proxy mode
        let local_stream = TcpStream::connect(format!("{}:{}", args.local_addr, args.local_port)).await?;
        info!("('{}') Connected to local service at {}:{}.", proxy_conn_id, args.local_addr, args.local_port);

        info!("('{}') Joining streams...", proxy_conn_id);
        join_streams(proxy_stream, local_stream).await?;
        info!("('{}') Streams joined and finished.", proxy_conn_id);
    }

    Ok(())
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

/// Handle HTTP/JSON-RPC requests over TCP stream
async fn execute_command_and_stream(
    mut proxy_stream: TcpStream,
    proxy_conn_id: &str,
    session_manager: Arc<Mutex<SessionManager>>,
    server_state: Arc<ServerState>,
) -> Result<()> {
    use serde_json::Value;

    info!("('{}') Starting HTTP/JSON-RPC handler", proxy_conn_id);

    // HTTP/1.1 keep-alive loop
    loop {
        // Parse HTTP request

        let request = HttpRequest::parse(&mut proxy_stream, proxy_conn_id).await?;

        info!("('{}') Received {} {}", proxy_conn_id, request.method.as_str(), request.path);
        // info!("('{}') Request body: {:?}", proxy_conn_id, request.body_as_json());
        let json = request.body_as_json().unwrap();
         let method_name = json.get("method").and_then(|m| m.as_str());
        let id = json.get("id");

        // Check if client wants to close connection
        let should_close = request.headers.get("connection")
            .map(|v| v.to_lowercase() == "close")
            .unwrap_or(false);

        // Route request
        let result = match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/agents") => {
                handle_list_agents(&mut proxy_stream, session_manager.clone()).await
            }
            ("GET", "/session/list") => {
                handle_list_sessions(&mut proxy_stream, session_manager.clone()).await
            }
            ("POST", "/acp/session/message") => {

                match method_name.unwrap()  {
                    "session/new" => {
                       handle_new_session(&mut proxy_stream, &request.body, session_manager.clone(), server_state.clone()).await
                    },
                    "session/prompt" => {
                        handle_prompt(&mut proxy_stream, &request.body, session_manager.clone()).await
                    },
                    _ => {
                        write_error_response(&mut proxy_stream, 404, "Not Found", "Method not found").await
                    }
                }
                
            }
            ("OPTIONS", _) => {
                handle_options(&mut proxy_stream).await
            }
            _ => {
                write_error_response(&mut proxy_stream, 404, "Not Found", "Endpoint not found").await
            }
        };

        if let Err(e) = result {
            error!("('{}') Error handling request: {}", proxy_conn_id, e);
            break;
        }

        // Exit loop if Connection: close
        if should_close {
            info!("('{}') Client requested connection close", proxy_conn_id);
            break;
        }
    }

    info!("('{}') HTTP handler finished", proxy_conn_id);
    Ok(())
}

/// Handle GET /api/agents
async fn handle_list_agents(
    stream: &mut TcpStream,
    session_manager: Arc<Mutex<SessionManager>>,
) -> Result<()> {
    let agents = session_manager.lock().await.list_agents();
    let response_json = json!({
        "agents": agents
    });
    write_json_response(stream, 200, "OK", &response_json.to_string()).await
}

/// Handle GET /session/list
async fn handle_list_sessions(
    stream: &mut TcpStream,
    session_manager: Arc<Mutex<SessionManager>>,
) -> Result<()> {
    let sessions = session_manager.lock().await.list_sessions();
    let response_json = json!({
        "sessions": sessions
    });
    write_json_response(stream, 200, "OK", &response_json.to_string()).await
}

/// Handle POST /session/new
async fn handle_new_session(
    stream: &mut TcpStream,
    body: &[u8],
    session_manager: Arc<Mutex<SessionManager>>,
    server_state: Arc<ServerState>,
) -> Result<()> {
    use serde_json::Value;

    // Parse JSON body
    // let body_str = String::from_utf8_lossy(body);
    // let json: Value = match serde_json::from_str(&body_str) {
    //     Ok(j) => j,
    //     Err(e) => {
    //         return write_error_response(stream, 400, "Bad Request", &format!("Invalid JSON: {}", e)).await;
    //     }
    // };

    // Call SessionManager to handle the request
    let mut manager = session_manager.lock().await;
    match manager.handle_new_session_request(body, server_state, session_manager.clone()).await {
        Ok(response_message) => {
            drop(manager);
            write_json_response(stream, 200, "OK", &response_message).await
        }
        Err(e) => {
            drop(manager);
            write_error_response(stream, 500, "Internal Server Error", &e).await
        }
    }
}

/// Handle POST /session/prompt
async fn handle_prompt(
    stream: &mut TcpStream,
    body: &[u8],
    session_manager: Arc<Mutex<SessionManager>>,
) -> Result<()> {
    use serde_json::Value;

    // Parse JSON body
    let body_str = String::from_utf8_lossy(body);
    let json: Value = match serde_json::from_str(&body_str) {
        Ok(j) => j,
        Err(e) => {
            return write_error_response(stream, 400, "Bad Request", &format!("Invalid JSON: {}", e)).await;
        }
    };

    // Call SessionManager to get the receiver
    let mut manager = session_manager.lock().await;
    match manager.handle_prompt_request(&json).await {
        Ok(receiver) => {
            drop(manager);
            // Stream SSE responses
            write_sse_stream(stream, receiver).await
        }
        Err(e) => {
            drop(manager);
            write_error_response(stream, 404, "Not Found", &e).await
        }
    }
}

/// Handle OPTIONS requests (CORS preflight)
async fn handle_options(stream: &mut TcpStream) -> Result<()> {
    let response = "HTTP/1.1 200 OK\r\n\
                    Access-Control-Allow-Origin: *\r\n\
                    Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
                    Access-Control-Allow-Headers: Content-Type\r\n\
                    Content-Length: 0\r\n\
                    Connection: keep-alive\r\n\
                    \r\n";
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

// We need to implement Clone for Args to use it in the spawned task.
impl Clone for Args {
    fn clone(&self) -> Self {
        Args {
            client_id: self.client_id.clone(),
            server_addr: self.server_addr.clone(),
            control_port: self.control_port,
            proxy_port: self.proxy_port,
            local_addr: self.local_addr.clone(),
            local_port: self.local_port,
            command_mode: self.command_mode,
            config: self.config.clone(),
            command_path: self.command_path.clone(),
            command_args: self.command_args.clone(),
        }
    }
}