use std::sync::Arc;
use crate::handlers::HandlerState;
use crate::router::HandlerContext;
use anyhow::Result;
use common::http::HttpResponse;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

pub async fn handle_agents(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    state.log("Listing all agents");
    let manager = state.session_manager.lock().await;
    let agents = manager.list_agents();
    let agents_json: Vec<Value> = agents.iter().map(|(name, config)| {
        serde_json::json!({
            "name": name,
            "command": config.command,
            "args": config.args,
            "env": config.env
        })
    }).collect();
    let response = serde_json::json!({
        "success": true,
        "agents": agents_json
    });
    println!("{}", response);

    let mut stream = ctx.stream;
    let _ = HttpResponse::ok().json(&response).send(&mut stream).await;

    Ok(HttpResponse::ok())

    // return Ok(HttpResponse::new(200).json(&response).send(ctx.stream));
}
pub async fn handle_session(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    // println!("handle_session")
    let json: Value = match ctx.request.body_as_json() {
        Ok(json) => json,
        Err(_) =>{
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": "Parse error"}
            });

            let mut stream = ctx.stream;
            let _ = HttpResponse::bad_request().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
    };

    let method = json.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = json.get("id").cloned();

    match method {
        "session/new" => {
            handle_session_new(ctx, state, json, id).await
        }
        "initialize" |  "session/set_mode" | "session/set_model" | "session/cancel" => {
            handle_session_single_message(ctx, state, json, id).await
        }
        "session/prompt" => {
            handle_session_prompt_sse(ctx, state, json, id).await
        }
        "session/list" => {
            state.log("Received session/list request");

            let sessions = state.session_manager.lock().await.list_sessions();
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "sessions": sessions }
            });

            state.pretty_print_message("[Server → Client]", &serde_json::to_string(&response).unwrap());
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
        _ => {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "Invalid params"}
            });

            let mut stream = ctx.stream;
            let _ = HttpResponse::bad_request().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        },
    }
    
}

/// SSE streaming example
async fn handle_sse_session_example(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    // let proxy_conn_id = &ctx.proxy_conn_id;
    let mut stream = ctx.stream;

    // Send SSE headers
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, PUT, DELETE, PATCH, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n\r\n").await?;
    stream.flush().await?;

    if stream
        .write_all(format!("data: {}\n\n", "-----------------------").as_bytes())
        .await
        .is_err()
    {
        return Ok(HttpResponse::ok());
    }
    stream.flush().await?;

    Ok(HttpResponse::ok())
}

async fn handle_session_single_message(ctx: HandlerContext, state: HandlerState, json: Value, id: Option<Value>) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "Invalid params"}
            });

            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
        
    };
    let session_id = match params.get("_meta").and_then(|m| m.get("sessionId")).and_then(|a| a.as_str()) {
        Some(session_id) => session_id,
        None => {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "Missing _meta.sessionId"}
            });

            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
    };

    let session_manager = state.session_manager.clone();
    let mut manager = session_manager.lock().await;


    let Some(stdin) = manager.get_session_stdin(&session_id) else {
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "Failed to get session stdin"}
        });
        let mut stream = ctx.stream;
        let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
        return  Ok(HttpResponse::ok())
    };

    let request_str = format!("{}\n", json.to_string());

    if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": format!("Failed to communicate with agent: {}", e)}
        });
        let mut stream = ctx.stream;
        let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
        return  Ok(HttpResponse::ok())
    }
    let _ = stdin.flush().await;

    let Some(mut receiver) = manager.subscribe_to_session(&session_id) else {
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "Failed to subscribe to session"}
        });
        let mut stream = ctx.stream;
        let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
        return  Ok(HttpResponse::ok())
    };
    drop(manager);

    match tokio::time::timeout(tokio::time::Duration::from_secs(30), receiver.recv()).await {
        Ok(Ok(message)) => {
            state.pretty_print_message("[Server → HTTP Client]", &message);
            let msg =  serde_json::from_str(&message).unwrap();
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&msg).send(&mut stream).await;
            return Ok(HttpResponse::ok())
        }
        Ok(Err(e)) => {
            let error_resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": format!("Broadcast error: {}", e)}
            });
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
        Err(_) => {
            let error_resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": "Timeout waiting for session/new response"}
            });
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
    }

   
}

async fn handle_session_new(ctx: HandlerContext, state: HandlerState, json: Value, id: Option<Value>) -> Result<HttpResponse> {
    let params = match json.get("params") {
        Some(p) => p,
        None => {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "Invalid params"}
            });

            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
        
    };
    println!("{:?}", params);
    let agent_name = match params.get("_meta").and_then(|m| m.get("agentName")).and_then(|a| a.as_str()) {
        Some(name) => name,
        None => {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "Missing _meta.agent_name"}
            });

            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&response).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
    };

    let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or(".").to_string();
    let session_manager = state.session_manager.clone();
    let mut manager = session_manager.lock().await;
    let temp_session_id = {
        
        match manager.create_session(agent_name, cwd, Arc::new(state.clone()), session_manager.clone()).await {
            Ok(id) => id,
            Err(e) => {
               
                let response = &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": e}
                });
                let mut stream = ctx.stream;
                let _ = HttpResponse::ok().json(&response).send(&mut stream).await;
               return  Ok(HttpResponse::ok())
            }
        }
    };

    state.log(&format!("Created session for agent: {}", agent_name));

    let Some(stdin) = manager.get_session_stdin(&temp_session_id) else {
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "Failed to get session stdin"}
        });
        let mut stream = ctx.stream;
        let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
        return  Ok(HttpResponse::ok())
    };

    let jsonrpc_request = serde_json::json!({"id": id, "jsonrpc": "2.0", "method": "session/new", "params": params});
    let request_str = format!("{}\n", jsonrpc_request.to_string());

    if let Err(e) = stdin.write_all(request_str.as_bytes()).await {
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": format!("Failed to communicate with agent: {}", e)}
        });
        let mut stream = ctx.stream;
        let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
        return  Ok(HttpResponse::ok())
    }
    let _ = stdin.flush().await;

    let Some(mut receiver) = manager.subscribe_to_session(&temp_session_id) else {
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "Failed to subscribe to session"}
        });
        let mut stream = ctx.stream;
        let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
        return  Ok(HttpResponse::ok())
    };
    drop(manager);

    match tokio::time::timeout(tokio::time::Duration::from_secs(30), receiver.recv()).await {
        Ok(Ok(message)) => {
            state.pretty_print_message("[Server → HTTP Client]", &message);
            let msg =  serde_json::from_str(&message).unwrap();
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&msg).send(&mut stream).await;
            return Ok(HttpResponse::ok())
        }
        Ok(Err(e)) => {
            let error_resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": format!("Broadcast error: {}", e)}
            });
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
        Err(_) => {
            let error_resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": "Timeout waiting for session/new response"}
            });
            let mut stream = ctx.stream;
            let _ = HttpResponse::ok().json(&error_resp).send(&mut stream).await;
            return  Ok(HttpResponse::ok())
        }
    }

   
}

async fn handle_session_prompt_sse(ctx: HandlerContext, state: HandlerState, json: Value, id: Option<Value>) -> Result<HttpResponse> {
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
    let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or(".").to_string();
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

    let mut stream = ctx.stream;

    // Send SSE headers
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
                {
                    return Ok(HttpResponse::ok());
                }
                if stream.flush().await.is_err() {
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