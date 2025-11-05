use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub path: String,
    pub sessions: Vec<String>,
    pub created_at: u64,
    pub most_recent_session: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub project_path: String,
    pub todo_data: Option<Value>,
    pub created_at: u64,
    pub first_message: Option<String>,
    pub message_timestamp: Option<String>,
    pub message_count: usize,
    pub status: String,
    pub total_duration: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingDirectory {
    pub path: String,
    #[serde(rename = "shortname")]
    pub short_name: String,
    #[serde(rename = "lastDate")]
    pub last_date: String,
    #[serde(rename = "conversationCount")]
    pub conversation_count: usize,
    pub AgentName: String,
}
