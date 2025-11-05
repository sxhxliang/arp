use crate::config::AgentConfig;
use crate::handlers::HandlerState;
use crate::jsonrpc;
use crate::router::HandlerContext;
use anyhow::Result;
use common::http::HttpResponse;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Helper to send JSON response
async fn send_json_response(
    mut stream: tokio::net::TcpStream,
    response: &Value,
) -> Result<HttpResponse> {
    let _ = HttpResponse::ok().json(response).send(&mut stream).await;
    Ok(HttpResponse::ok())
}

/// Helper to send error response
async fn send_error(
    stream: tokio::net::TcpStream,
    id: Option<&Value>,
    code: i32,
    message: &str,
) -> Result<HttpResponse> {
    send_json_response(stream, &jsonrpc::error_response(id, code, message)).await
}

/// Helper to write to session stdin and subscribe
async fn write_to_session_and_subscribe(
    manager: &mut tokio::sync::MutexGuard<'_, crate::session::SessionManager>,
    session_id: &str,
    request: &Value,
) -> Result<tokio::sync::broadcast::Receiver<String>, &'static str> {
    let stdin = manager
        .get_session_stdin(session_id)
        .ok_or("Failed to get session stdin")?;

    stdin
        .write_all(format!("{}\n", request).as_bytes())
        .await
        .map_err(|_| "Failed to write to session")?;
    let _ = stdin.flush().await;

    manager
        .subscribe_to_session(session_id)
        .ok_or("Failed to subscribe to session")
}

/// Helper to wait for session response with timeout
async fn wait_for_response(
    receiver: &mut tokio::sync::broadcast::Receiver<String>,
    id: Option<&Value>,
) -> Result<Value, Value> {
    match tokio::time::timeout(tokio::time::Duration::from_secs(30), receiver.recv()).await {
        Ok(Ok(message)) => serde_json::from_str(&message)
            .map_err(|_| jsonrpc::error_response(id, -32603, "Invalid JSON response")),
        Ok(Err(e)) => Err(jsonrpc::error_response(
            id,
            -32603,
            &format!("Broadcast error: {}", e),
        )),
        Err(_) => Err(jsonrpc::error_response(
            id,
            -32603,
            "Timeout waiting for response",
        )),
    }
}

pub async fn handle_agents(
    ctx: HandlerContext,
    state: HandlerState,
    method: &str,
) -> Result<HttpResponse> {
    state.log("Listing all agents");
    let mut manager = state.session_manager.lock().await;

    let response = match method {
        "list" => {
            let agents = manager.list_agents();

            let agents_json: Vec<Value> = agents
                .iter()
                .map(|(name, config)| {
                    serde_json::json!({
                        "name": name,
                        "command": config.command,
                        "args": config.args,
                        "env": config.env
                    })
                })
                .collect();

            serde_json::json!({"success": true, "agents": agents_json})
        }
        "add" => {
            let json: Value = match ctx.request.body_as_json() {
                Ok(json) => json,
                Err(e) => {
                    let error = serde_json::json!({
                        "success": false,
                        "error": format!("Invalid JSON: {}", e)
                    });

                    return send_json_response(ctx.stream, &error).await;
                }
            };
            let name = match json.get("name").and_then(|n| n.as_str()) {
                Some(n) => n.to_string(),
                None => {
                    let error = serde_json::json!({
                        "success": false,
                        "error": "Missing 'name' field"
                    });
                    return send_json_response(ctx.stream, &error).await;
                }
            };

            let agent_config: AgentConfig = match serde_json::from_value(json.clone()) {
                Ok(config) => config,
                Err(e) => {
                    let error = serde_json::json!({
                        "success": false,
                        "error": format!("Invalid agent configuration: {}", e)
                    });
                    return send_json_response(ctx.stream, &error).await;
                }
            };
            match manager.add_agent(name, agent_config) {
                Ok(_) => serde_json::json!({"success": true, "agents": json}),
                Err(_) => serde_json::json!({"success": false, "agents": json}),
            }
        }
        "delete" => {
            let name = &ctx.request.path.trim_start_matches("/api/agents/");
            match manager.delete_agent(name) {
                Ok(_) => json!({"success": true}),
                Err(e) => {
                    let error = json!({
                        "success": false,
                        "error": format!("Invalid agent configuration: {}", e)
                    });
                    error
                    // return send_json_response(ctx.stream, &error).await
                }
            }
        }
        _ => {
            serde_json::json!({"success": false, "error": "Invalid command"})
        }
    };

    send_json_response(ctx.stream, &response).await
}
pub async fn handle_session(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    let json: Value = match ctx.request.body_as_json() {
        Ok(json) => json,
        Err(_) => return send_error(ctx.stream, None, -32700, "Parse error").await,
    };

    let method = json.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = json.get("id").cloned();

    match method {
        "session/new" => handle_session_new(ctx, state, json, id.as_ref()).await,
        "initialize" | "session/set_mode" | "session/set_model" | "session/cancel" => {
            handle_session_single_message(ctx, state, json, id.as_ref()).await
        }
        "session/prompt" => handle_session_prompt_sse(ctx, state, json, id.as_ref()).await,
        "session/list" => {
            state.log("Received session/list request");
            let sessions = state.session_manager.lock().await.list_sessions();
            let response =
                jsonrpc::success_response(id.as_ref(), serde_json::json!({"sessions": sessions}));
            state.pretty_print_message(
                "[Server → Client]",
                &serde_json::to_string(&response).unwrap(),
            );
            send_json_response(ctx.stream, &response).await
        }
        _ => send_error(ctx.stream, id.as_ref(), -32602, "Invalid method").await,
    }
}

async fn handle_session_single_message(
    ctx: HandlerContext,
    state: HandlerState,
    json: Value,
    id: Option<&Value>,
) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => return send_error(ctx.stream, id, -32602, "Invalid params").await,
    };

    let session_id = match jsonrpc::extract_session_id(params) {
        Some(s) => s,
        None => return send_error(ctx.stream, id, -32602, "Missing _meta.sessionId").await,
    };

    let mut manager = state.session_manager.lock().await;

    let mut receiver = match write_to_session_and_subscribe(&mut manager, session_id, &json).await {
        Ok(rx) => rx,
        Err(msg) => return send_error(ctx.stream, id, -32603, msg).await,
    };
    drop(manager);

    match wait_for_response(&mut receiver, id).await {
        Ok(msg) => {
            state.pretty_print_message("[Server → HTTP Client]", &msg.to_string());
            send_json_response(ctx.stream, &msg).await
        }
        Err(error) => send_json_response(ctx.stream, &error).await,
    }
}

async fn handle_session_new(
    ctx: HandlerContext,
    state: HandlerState,
    json: Value,
    id: Option<&Value>,
) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => return send_error(ctx.stream, id, -32602, "Invalid params").await,
    };

    let agent_name = match jsonrpc::extract_agent_name(params) {
        Some(name) => name,
        None => return send_error(ctx.stream, id, -32602, "Missing _meta.agentName").await,
    };

    let cwd = params
        .get("cwd")
        .and_then(|c| c.as_str())
        .unwrap_or(".")
        .to_string();
    let mut manager = state.session_manager.lock().await;

    let session_id = match manager
        .create_session(
            agent_name,
            cwd,
            Arc::new(state.clone()),
            state.session_manager.clone(),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => return send_error(ctx.stream, id, -32603, &e).await,
    };

    state.log(&format!("Created session for agent: {}", agent_name));

    let request =
        serde_json::json!({"id": id, "jsonrpc": "2.0", "method": "session/new", "params": params});
    let mut receiver =
        match write_to_session_and_subscribe(&mut manager, &session_id, &request).await {
            Ok(rx) => rx,
            Err(msg) => return send_error(ctx.stream, id, -32603, msg).await,
        };
    drop(manager);

    match wait_for_response(&mut receiver, id).await {
        Ok(msg) => {
            state.pretty_print_message("[Server → HTTP Client]", &msg.to_string());
            send_json_response(ctx.stream, &msg).await
        }
        Err(error) => send_json_response(ctx.stream, &error).await,
    }
}

async fn handle_session_prompt_sse(
    ctx: HandlerContext,
    state: HandlerState,
    json: Value,
    id: Option<&Value>,
) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => {
            return Ok(HttpResponse::new(400).json(&jsonrpc::error_response(
                id,
                -32602,
                "Invalid params",
            )));
        }
    };

    let session_id = match jsonrpc::extract_session_id(params) {
        Some(s) => s,
        None => {
            return Ok(HttpResponse::new(400).json(&jsonrpc::error_response(
                id,
                -32602,
                "Missing sessionId",
            )));
        }
    };

    let mut manager = state.session_manager.lock().await;
    let mut receiver = match write_to_session_and_subscribe(&mut manager, session_id, &json).await {
        Ok(rx) => rx,
        Err(_) => {
            return Ok(HttpResponse::new(404).json(&jsonrpc::error_response(
                id,
                -32001,
                "Session not found",
            )));
        }
    };
    drop(manager);

    let mut stream = ctx.stream;
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, PUT, DELETE, PATCH, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n\r\n").await?;
    stream.flush().await?;

    loop {
        match receiver.recv().await {
            Ok(message) => {
                let is_end = if let Ok(j) = serde_json::from_str::<Value>(&message) {
                    j.get("result")
                        .and_then(|r| r.get("stopReason"))
                        .and_then(|s| s.as_str())
                        .map(|s| s == "end_turn")
                        .unwrap_or(false)
                } else {
                    false
                };

                if stream
                    .write_all(format!("data: {}\n\n", message).as_bytes())
                    .await
                    .is_err()
                    || stream.flush().await.is_err()
                {
                    break;
                }

                if is_end {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    Ok(HttpResponse::ok())
}
