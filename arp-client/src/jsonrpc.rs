use serde_json::Value;

/// Extract agentName from JSON-RPC params._meta
pub fn extract_agent_name(params: &Value) -> Option<&str> {
    params.get("_meta")?.get("agentName")?.as_str()
}

/// Extract sessionId from params (supports both _meta.sessionId and sessionId)
pub fn extract_session_id(params: &Value) -> Option<&str> {
    params
        .get("_meta")
        .and_then(|m| m.get("sessionId"))
        .and_then(|s| s.as_str())
        .or_else(|| params.get("sessionId")?.as_str())
}

/// Create JSON-RPC error response
pub fn error_response(id: Option<&Value>, code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

/// Create JSON-RPC success response
pub fn success_response(id: Option<&Value>, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}
