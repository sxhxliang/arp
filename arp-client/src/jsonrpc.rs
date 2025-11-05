use serde_json::Value;

/// Extract agentName from JSON-RPC params
pub fn extract_agent_name(json: &Value) -> Option<String> {
    json["params"]["_meta"]["agentName"].as_str().map(String::from)
}

/// Extract sessionId from JSON-RPC message (supports multiple locations)
pub fn extract_session_id(json: &Value) -> Option<String> {
    json["params"]["_meta"]["sessionId"].as_str()
        .or_else(|| json["params"]["sessionId"].as_str())
        .or_else(|| json["result"]["_meta"]["sessionId"].as_str())
        .map(String::from)
}

/// Create JSON-RPC error response
pub fn create_error_response(id: Option<i64>, code: i32, message: &str) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

/// Create JSON-RPC error response (Value version for http module)
pub fn jsonrpc_error(id: Option<&Value>, code: i32, message: &str) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Extract sessionId from params (supports both _meta.sessionId and sessionId)
pub fn get_session_id(params: &Value) -> Option<&str> {
    params.get("_meta").and_then(|m| m.get("sessionId")).and_then(|s| s.as_str())
        .or_else(|| params.get("sessionId").and_then(|s| s.as_str()))
}
