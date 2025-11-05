use crate::jsonrpc::{create_error_response, extract_agent_name, extract_session_id};
use crate::session::SessionManager;
use crate::handlers::HandlerState;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Mutex};

/// Forward a JSON-RPC request to a child process session
/// Returns (error_response_or_empty, output_receiver)
pub async fn forward_to_session(
    content: &str,
    session_id: &str,
    method_name: &str,
    session_manager: Arc<Mutex<SessionManager>>,
    state: Arc<HandlerState>,
    id: Option<i64>,
) -> Option<(String, broadcast::Receiver<String>)> {
    state.log(&format!("{} for session: {}", method_name, session_id));

    let mut manager = session_manager.lock().await;
    let output_rx = manager.subscribe_to_session(session_id);

    if let Some(stdin) = manager.get_session_stdin(session_id) {
        if let Err(e) = stdin.write_all(content.as_bytes()).await {
            state.log_error(&format!("Failed to write to child stdin: {}", e));
            let error = create_error_response(id, -32603, "Failed to communicate with agent");
            return Some((error, broadcast::channel(1).1));
        }
        if let Err(e) = stdin.flush().await {
            state.log_error(&format!("Failed to flush stdin: {}", e));
        }

        return output_rx.map(|rx| (String::new(), rx));
    }

    state.log_error(&format!("Session not found: {}", session_id));
    let error = create_error_response(id, -32001, "Session not found");
    Some((error, broadcast::channel(1).1))
}

/// Process a JSON-RPC message and route it to the appropriate session
pub async fn process_message(
    content: String,
    session_manager: Arc<Mutex<SessionManager>>,
    state: Arc<HandlerState>,
) -> Option<(String, broadcast::Receiver<String>)> {
    let json: Value = match serde_json::from_str(&content) {
        Ok(json) => json,
        Err(e) => {
            state.log_error(&format!("Failed to parse JSON: {}", e));
            let error = create_error_response(None, -32700, "Parse error");
            return Some((error, broadcast::channel(1).1));
        }
    };

    let method = json.get("method").and_then(|m| m.as_str());
    let id = json.get("id").and_then(|i| i.as_i64());

    if method == Some("session/new") {
        let agent_name = match extract_agent_name(&json) {
            Some(name) => name,
            None => {
                state.log_error("Missing agentName in session/new request");
                let error = create_error_response(id, -32602, "Missing agentName in params._meta");
                return Some((error, broadcast::channel(1).1));
            }
        };

        let cwd = json.get("params")
            .and_then(|p| p.get("cwd"))
            .and_then(|c| c.as_str())
            .unwrap_or(".")
            .to_string();

        let mut manager = session_manager.lock().await;
        match manager.create_session(&agent_name, cwd, state.clone(), session_manager.clone()).await {
            Ok(temp_session_id) => {
                state.log(&format!("Created session for agent: {}", agent_name));

                let output_rx = manager.subscribe_to_session(&temp_session_id);

                if let Some(stdin) = manager.get_session_stdin(&temp_session_id) {
                    if let Err(e) = stdin.write_all(content.as_bytes()).await {
                        state.log_error(&format!("Failed to write to child stdin: {}", e));
                        let error = create_error_response(id, -32603, "Failed to communicate with agent");
                        return Some((error, broadcast::channel(1).1));
                    }
                    if let Err(e) = stdin.flush().await {
                        state.log_error(&format!("Failed to flush stdin: {}", e));
                    }

                    return output_rx.map(|rx| (String::new(), rx));
                }
            }
            Err(e) => {
                state.log_error(&format!("Failed to create session: {}", e));
                let error = create_error_response(id, -32603, &e);
                return Some((error, broadcast::channel(1).1));
            }
        }
    }
    else if matches!(method, Some("session/load") | Some("initialize") | Some("session/set_mode") | Some("session/set_model")) {
        let session_id = match extract_session_id(&json) {
            Some(id) => id,
            None => {
                state.log_error(&format!("Missing sessionId in {} request", method.unwrap_or("unknown")));
                let error = create_error_response(id, -32602, "Missing sessionId in params._meta or params");
                return Some((error, broadcast::channel(1).1));
            }
        };

        let method_name = method.unwrap_or("unknown method");
        return forward_to_session(&content, &session_id, method_name, session_manager, state, id).await;
    }
    else if method == Some("session/list") {
        state.log("Received session/list request");

        let sessions = session_manager.lock().await.list_sessions();
        let response = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "sessions": sessions }
        })).unwrap();

        state.pretty_print_message("[Server → Client]", &response);

        return Some((response, broadcast::channel(1).1));
    }
    else if method == Some("session/reconnect") {
        let session_id = match extract_session_id(&json) {
            Some(id) => id,
            None => {
                state.log_error("Missing sessionId in session/reconnect request");
                let error = create_error_response(id, -32602, "Missing sessionId in params._meta or params");
                return Some((error, broadcast::channel(1).1));
            }
        };

        state.log(&format!("Reconnecting to session: {}", session_id));

        let manager = session_manager.lock().await;

        if let Some(receiver) = manager.subscribe_to_session(&session_id) {
            let session_info = manager.sessions.get(&session_id).map(|s| {
                let updated = s.updated_at.try_lock().map(|u| u.clone()).unwrap_or_else(|_| s.created_at.clone());
                serde_json::json!({
                    "sessionId": s.session_id,
                    "createdAt": s.created_at,
                    "updatedAt": updated,
                    "cwd": s.cwd,
                    "title": s.title,
                    "_meta": {"agentName": &s.agent_name}
                })
            });

            let response = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "success": true,
                    "session": session_info
                }
            })).unwrap();

            state.pretty_print_message("[Server → Client]", &response);

            return Some((response, receiver));
        } else {
            state.log_error(&format!("Session not found: {}", session_id));
            let error = create_error_response(id, -32001, "Session not found");
            return Some((error, broadcast::channel(1).1));
        }
    }
    else if method == Some("session/cancel") {
        let session_id = match extract_session_id(&json) {
            Some(id) => id,
            None => {
                state.log_error("Missing sessionId in session/cancel request");
                let error = create_error_response(id, -32602, "Missing sessionId in params._meta or params");
                return Some((error, broadcast::channel(1).1));
            }
        };

        state.log(&format!("Closing session: {}", session_id));

        let mut manager = session_manager.lock().await;
        manager.close_session(&session_id, state.clone()).await;

        let response = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "success": true }
        })).unwrap();

        state.pretty_print_message("[Server → Client]", &response);

        return Some((response, broadcast::channel(1).1));
    }
    else {
        let session_id = match extract_session_id(&json) {
            Some(id) => id,
            None => {
                state.log_error("Missing sessionId in request");
                let error = create_error_response(id, -32602, "Missing sessionId in params._meta or params");
                return Some((error, broadcast::channel(1).1));
            }
        };

        let mut manager = session_manager.lock().await;
        if let Some(stdin) = manager.get_session_stdin(&session_id) {
            if let Err(e) = stdin.write_all(content.as_bytes()).await {
                state.log_error(&format!("Failed to write to child stdin: {}", e));
                let error = create_error_response(id, -32603, "Failed to communicate with session");
                return Some((error, broadcast::channel(1).1));
            } else if let Err(e) = stdin.flush().await {
                state.log_error(&format!("Failed to flush stdin: {}", e));
            }
        } else {
            state.log_error(&format!("Session not found: {}", session_id));
            let error = create_error_response(id, -32001, "Session not found");
            return Some((error, broadcast::channel(1).1));
        }
    }

    None
}
