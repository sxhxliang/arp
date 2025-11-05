use std::sync::Arc;
use crate::handlers::HandlerState;
use crate::router::HandlerContext;
use anyhow::Result;
use common::http::HttpResponse;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

pub async fn handle_session(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    let json: Value = match ctx.request.body_as_json() {
        Ok(json) => json,
        Err(_) => return Ok(HttpResponse::new(400).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "Parse error"}
        }))),
    };

    let method = json.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = json.get("id").cloned();

    match method {
        "session/new" => handle_session_new(json, id, state).await,
        "session/prompt" => handle_session_prompt(ctx, json, id, state).await,
        _ => Ok(HttpResponse::new(404).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "Method not found"}
        }))),
    }
}

async fn handle_session_new(json: Value, id: Option<Value>, state: HandlerState) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => return Ok(HttpResponse::new(400).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32602, "message": "Invalid params"}
        }))),
    };

    let agent_name = match params.get("_meta").and_then(|m| m.get("agent_name")).and_then(|a| a.as_str()) {
        Some(name) => name,
        None => return Ok(HttpResponse::new(400).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32602, "message": "Missing _meta.agent_name"}
        }))),
    };

    let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or(".").to_string();
    let session_manager = state.session_manager.clone();

    let temp_session_id = {
        let mut manager = session_manager.lock().await;
        match manager.create_session(agent_name, cwd, Arc::new(state.clone()), session_manager.clone()).await {
            Ok(id) => id,
            Err(e) => return Ok(HttpResponse::new(500).json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": e}
            }))),
        }
    };

    let request_str = format!("{}\n", serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "session/new", "params": params}));

    {
        let mut manager = session_manager.lock().await;
        if let Some(stdin) = manager.get_session_stdin(&temp_session_id) {
            if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                return Ok(HttpResponse::new(500).json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": format!("Failed to write: {}", e)}
                })));
            }
            let _ = stdin.flush().await;
        } else {
            return Ok(HttpResponse::new(500).json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": "Failed to get session stdin"}
            })));
        }
    }

    let mut receiver = {
        let manager = session_manager.lock().await;
        match manager.subscribe_to_session(&temp_session_id) {
            Some(rx) => rx,
            None => return Ok(HttpResponse::new(500).json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": "Failed to subscribe"}
            }))),
        }
    };

    match tokio::time::timeout(tokio::time::Duration::from_secs(30), receiver.recv()).await {
        Ok(Ok(message)) => Ok(HttpResponse::new(200).body(message.into_bytes())),
        Ok(Err(_)) => Ok(HttpResponse::new(500).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "Broadcast error"}
        }))),
        Err(_) => Ok(HttpResponse::new(504).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "Timeout"}
        }))),
    }
}

async fn handle_session_prompt(mut ctx: HandlerContext, json: Value, id: Option<Value>, state: HandlerState) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => return Ok(HttpResponse::new(400).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32602, "message": "Invalid params"}
        }))),
    };

    let session_id = match params.get("sessionId").and_then(|s| s.as_str()) {
        Some(id) => id.to_string(),
        None => return Ok(HttpResponse::new(400).json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32602, "message": "Missing sessionId"}
        }))),
    };

    let session_manager = state.session_manager.clone();
    let request_str = format!("{}\n", json);

    {
        let mut manager = session_manager.lock().await;
        if let Some(stdin) = manager.get_session_stdin(&session_id) {
            if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
                return Ok(HttpResponse::new(500).json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": format!("Failed to write: {}", e)}
                })));
            }
            let _ = stdin.flush().await;
        } else {
            return Ok(HttpResponse::new(404).json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32001, "message": "Session not found"}
            })));
        }
    }

    let mut receiver = {
        let manager = session_manager.lock().await;
        match manager.subscribe_to_session(&session_id) {
            Some(rx) => rx,
            None => return Ok(HttpResponse::new(404).json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32001, "message": "Session not found"}
            }))),
        }
    };

    let stream = &mut ctx.stream;
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n").await?;
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

                stream.write_all(format!("data: {}\n\n", message).as_bytes()).await?;
                stream.flush().await?;

                if is_end {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    Ok(HttpResponse::ok())
}
