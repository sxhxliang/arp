pub use crate::agentx::types::{Project, Session, WorkingDirectory};
use crate::agentx::utils::metadata_timestamp;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const AGENT_NAME: &str = "gemini";

#[derive(Debug, Clone, Deserialize)]
struct GeminiSessionFile {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "projectPath")]
    project_path: Option<String>,
    #[serde(rename = "workingDirectory")]
    working_directory: Option<String>,
    #[serde(rename = "projectName")]
    project_name: Option<String>,
    #[serde(rename = "startTime")]
    start_time: Option<String>,
    #[serde(rename = "lastUpdated")]
    last_updated: Option<String>,
    status: Option<String>,
    messages: Option<Vec<GeminiMessage>>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct GeminiMessage {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    #[serde(default)]
    message_type: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Value,
}

#[derive(Debug)]
struct GeminiSessionSummary {
    id: String,
    project_path: Option<String>,
    created_at: u64,
    last_updated: Option<u64>,
    first_message: Option<String>,
    message_timestamp: Option<String>,
    message_count: usize,
    status: String,
    total_duration: Option<f64>,
}

#[derive(Debug)]
struct GeminiProjectData {
    id: String,
    path: String,
    sessions: Vec<GeminiSessionSummary>,
    created_at: u64,
    most_recent_session: Option<u64>,
}

fn gemini_tmp_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let tmp_dir = home.join(".gemini").join("tmp");
    tmp_dir
        .canonicalize()
        .context("Could not find ~/.gemini/tmp directory")
}

fn is_project_dir(entry: &fs::DirEntry) -> bool {
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    let file_name = entry.file_name();
    let Some(name) = file_name.to_str() else {
        return false;
    };
    name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit())
}

fn parse_timestamp(value: &Option<String>) -> Option<DateTime<Utc>> {
    value
        .as_deref()
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn extract_first_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Array(values) => values.iter().find_map(extract_first_text),
        Value::Object(map) => {
            for key in ["text", "content", "message", "value"] {
                if let Some(v) = map.get(key)
                    && let Some(text) = extract_first_text(v)
                {
                    return Some(text);
                }
            }
            map.values().find_map(extract_first_text)
        }
        _ => None,
    }
}

fn read_session_value(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&content) {
        Ok(value) => Some(value),
        Err(_) => {
            let values: Vec<Value> = content
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    (!line.is_empty()).then(|| serde_json::from_str::<Value>(line).ok())?
                })
                .collect();
            Some(Value::Array(values))
        }
    }
}

fn parse_session_file(path: &Path) -> Option<(GeminiSessionFile, Value)> {
    let value = read_session_value(path)?;

    if let Some(obj) = value.as_object()
        && obj.contains_key("sessionId")
        && let Ok(session) = serde_json::from_value::<GeminiSessionFile>(value.clone())
    {
        return Some((session, value));
    }

    if let Value::Array(entries) = &value {
        for entry in entries {
            if let Some(obj) = entry.as_object()
                && obj.contains_key("sessionId")
                && let Ok(session) = serde_json::from_value::<GeminiSessionFile>(entry.clone())
            {
                return Some((session, Value::Object(obj.clone())));
            }
        }
    }

    None
}

fn infer_project_path(dir: &Path, summaries: &[GeminiSessionSummary]) -> Option<String> {
    for summary in summaries {
        if let Some(path) = &summary.project_path
            && !path.trim().is_empty()
        {
            return Some(path.clone());
        }
    }

    let logs_path = dir.join("logs.json");
    let Ok(content) = fs::read_to_string(&logs_path) else {
        return None;
    };

    if let Ok(value) = serde_json::from_str::<Value>(&content) {
        if let Some(array) = value.as_array() {
            for entry in array {
                if let Some(path) = extract_candidate_path(entry) {
                    return Some(path);
                }
            }
        } else if let Some(path) = extract_candidate_path(&value) {
            return Some(path);
        }
    } else {
        for line in content.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line)
                && let Some(path) = extract_candidate_path(&value)
            {
                return Some(path);
            }
        }
    }

    None
}

fn extract_candidate_path(value: &Value) -> Option<String> {
    value
        .get("projectPath")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .or_else(|| {
            value
                .get("workingDirectory")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        })
        .or_else(|| {
            value
                .get("cwd")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        })
        .filter(|s| !s.trim().is_empty())
}

fn collect_sessions(project_dir: &Path) -> Vec<GeminiSessionSummary> {
    let chats_dir = project_dir.join("chats");
    let entries = match fs::read_dir(&chats_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }

            let metadata = fs::metadata(&path).ok()?;
            let created_at = metadata_timestamp(&metadata);

            let (session_file, value) = parse_session_file(&path)?;
            let session_id = session_file.session_id.clone()?;
            let project_path = session_file
                .project_path
                .clone()
                .or(session_file.working_directory.clone())
                .or(session_file.project_name.clone());

            let start_time = parse_timestamp(&session_file.start_time);
            let end_time = parse_timestamp(&session_file.last_updated);
            let total_duration = match (start_time, end_time) {
                (Some(start), Some(end)) => Some((end - start).num_seconds().max(0) as f64),
                _ => None,
            };

            let last_updated = end_time.map(|dt| dt.timestamp() as u64);

            let messages = session_file.messages.clone().unwrap_or_else(|| {
                if let Some(array) = value.get("messages").and_then(|v| v.as_array()) {
                    array
                        .iter()
                        .filter_map(|v| serde_json::from_value::<GeminiMessage>(v.clone()).ok())
                        .collect()
                } else {
                    Vec::new()
                }
            });

            let first_message = messages.iter().find_map(|msg| {
                let is_user = matches!(
                    (msg.message_type.as_deref(), msg.role.as_deref()),
                    (Some("user"), _) | (_, Some("user"))
                );
                if is_user {
                    extract_first_text(&msg.content)
                } else {
                    None
                }
            });

            let message_timestamp = if let Some(last) = messages.iter().rev().find_map(|msg| {
                msg.timestamp
                    .clone()
                    .or_else(|| session_file.last_updated.clone())
            }) {
                Some(last)
            } else {
                session_file.last_updated.clone()
            };

            let status = session_file
                .status
                .clone()
                .or_else(|| {
                    session_file
                        .extra
                        .get("status")
                        .and_then(|v| v.as_str().map(String::from))
                })
                .unwrap_or_else(|| "unknown".to_string());

            Some(GeminiSessionSummary {
                id: session_id,
                project_path,
                created_at,
                last_updated,
                first_message,
                message_timestamp,
                message_count: messages.len(),
                status,
                total_duration,
            })
        })
        .collect()
}

fn load_project_data(dir: &Path) -> Option<GeminiProjectData> {
    let sessions = collect_sessions(dir);
    if sessions.is_empty() {
        return None;
    }

    let project_path =
        infer_project_path(dir, &sessions).unwrap_or_else(|| dir.to_string_lossy().into_owned());

    let dir_metadata = fs::metadata(dir).ok();
    let created_at = sessions
        .iter()
        .map(|s| s.created_at)
        .min()
        .or_else(|| dir_metadata.as_ref().map(metadata_timestamp))
        .unwrap_or(0);

    let most_recent_session = sessions
        .iter()
        .filter_map(|s| s.last_updated.or(Some(s.created_at)))
        .max();

    Some(GeminiProjectData {
        id: dir.file_name()?.to_string_lossy().into_owned(),
        path: project_path,
        sessions,
        created_at,
        most_recent_session,
    })
}

pub async fn list_projects() -> Result<Vec<Project>, String> {
    let tmp_dir = gemini_tmp_dir().map_err(|e| e.to_string())?;

    let mut projects: Vec<Project> = fs::read_dir(&tmp_dir)
        .map_err(|e| format!("Failed to read Gemini tmp directory: {}", e))?
        .flatten()
        .filter(is_project_dir)
        .filter_map(|entry| load_project_data(&entry.path()))
        .map(|data| Project {
            id: data.id,
            path: data.path,
            sessions: data.sessions.iter().map(|s| s.id.clone()).collect(),
            created_at: data.created_at,
            most_recent_session: data.most_recent_session,
        })
        .collect();

    projects.sort_by(
        |a, b| match (a.most_recent_session, b.most_recent_session) {
            (Some(at), Some(bt)) => bt.cmp(&at),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b.created_at.cmp(&a.created_at),
        },
    );

    Ok(projects)
}

pub async fn get_all_sessions(
    limit: Option<usize>,
    offset: Option<usize>,
    project_path: Option<String>,
) -> Result<Vec<Session>, String> {
    let tmp_dir = gemini_tmp_dir().map_err(|e| e.to_string())?;
    let mut sessions: Vec<(u64, Session)> = fs::read_dir(&tmp_dir)
        .map_err(|e| format!("Failed to read Gemini tmp directory: {}", e))?
        .flatten()
        .filter(is_project_dir)
        .filter_map(|entry| load_project_data(&entry.path()))
        .filter(|data| {
            project_path
                .as_ref()
                .map(|path| path == &data.path)
                .unwrap_or(true)
        })
        .flat_map(|data| {
            data.sessions.into_iter().map(move |summary| {
                let session = Session {
                    id: summary.id.clone(),
                    project_id: data.id.clone(),
                    project_path: data.path.clone(),
                    todo_data: None,
                    created_at: summary.created_at,
                    first_message: summary.first_message.clone(),
                    message_timestamp: summary.message_timestamp.clone(),
                    message_count: summary.message_count,
                    status: summary.status.clone(),
                    total_duration: summary.total_duration,
                };
                (summary.last_updated.unwrap_or(summary.created_at), session)
            })
        })
        .collect();

    sessions.sort_by(|a, b| b.0.cmp(&a.0));

    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(sessions.len());

    let filtered = sessions
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, session)| session)
        .collect();

    Ok(filtered)
}

pub async fn load_session_by_id(session_id: String) -> Result<Vec<Value>, String> {
    let tmp_dir = gemini_tmp_dir().map_err(|e| e.to_string())?;

    let normalized_id = session_id.trim_end_matches(".json").to_string();

    let mut candidates = Vec::new();

    for entry in fs::read_dir(&tmp_dir)
        .map_err(|e| format!("Failed to read Gemini tmp directory: {}", e))?
        .flatten()
        .filter(is_project_dir)
    {
        let project_path = entry.path();
        let chats_dir = project_path.join("chats");

        let Ok(chats) = fs::read_dir(&chats_dir) else {
            continue;
        };

        for chat in chats.flatten() {
            let path = chat.path();
            if !path.is_file() {
                continue;
            }

            if let Some((session_file, _)) = parse_session_file(&path)
                && session_file.session_id.as_deref() == Some(normalized_id.as_str())
            {
                candidates.push(path);
            }
        }
    }

    let session_path = candidates
        .into_iter()
        .next()
        .ok_or_else(|| format!("Session file not found for session ID: {}", normalized_id))?;

    let content = fs::read_to_string(&session_path)
        .map_err(|e| format!("Failed to read session file: {}", e))?;

    if let Ok(value) = serde_json::from_str::<Value>(&content) {
        if let Some(messages) = value.get("messages").and_then(|v| v.as_array()) {
            return Ok(messages.clone());
        }
        if value.is_array()
            && let Ok(messages) = serde_json::from_value::<Vec<Value>>(value)
        {
            return Ok(messages);
        }
    }

    let messages: Vec<Value> = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| serde_json::from_str::<Value>(line).ok())?
        })
        .collect();

    Ok(messages)
}

pub async fn delete_session_by_id(session_id: String) -> Result<(), String> {
    let tmp_dir = gemini_tmp_dir().map_err(|e| e.to_string())?;
    let normalized_id = session_id.trim_end_matches(".json").to_string();

    for entry in fs::read_dir(&tmp_dir)
        .map_err(|e| format!("Failed to read Gemini tmp directory: {}", e))?
        .flatten()
        .filter(is_project_dir)
    {
        let project_path = entry.path();
        let chats_dir = project_path.join("chats");

        let Ok(chats) = fs::read_dir(&chats_dir) else {
            continue;
        };

        for chat in chats.flatten() {
            let path = chat.path();
            if !path.is_file() {
                continue;
            }

            if let Some((session_file, _)) = parse_session_file(&path)
                && session_file.session_id.as_deref() == Some(normalized_id.as_str())
            {
                fs::remove_file(&path)
                    .map_err(|e| format!("Failed to delete session file: {}", e))?;
                return Ok(());
            }
        }
    }

    Err(format!(
        "Session file not found for session ID: {}",
        normalized_id
    ))
}

pub async fn get_working_directories() -> Result<Vec<WorkingDirectory>, String> {
    let projects = list_projects().await?;

    let directories = projects
        .into_iter()
        .map(|project| {
            let path_str = project.path.clone();
            let short_name = Path::new(&path_str)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| project.id.clone());

            let last_date = project
                .most_recent_session
                .and_then(|ts| Utc.timestamp_opt(ts as i64, 0).single())
                .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
                .to_rfc3339();

            WorkingDirectory {
                path: path_str,
                short_name,
                last_date,
                conversation_count: project.sessions.len(),
                AgentName: AGENT_NAME.to_string(),
            }
        })
        .collect();

    Ok(directories)
}
