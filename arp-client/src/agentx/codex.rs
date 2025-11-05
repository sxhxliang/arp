use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::SystemTime;

pub use crate::agentx::types::{Project, Session, WorkingDirectory};
use crate::agentx::utils::{metadata_timestamp, should_filter_message_text};

const AGENT_NAME: &str = "Codex";

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MessageField {
    Single(MessageContent),
    Multiple(Vec<MessageContent>),
}

#[derive(Debug, Deserialize)]
struct JsonlMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    message: Option<MessageField>,
    timestamp: Option<String>,
    payload: Option<SessionMetaPayload>,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageContent {
    role: Option<String>,
    content: Option<ContentField>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContentField {
    Text(String),
    Items(Vec<ContentItem>),
}

#[derive(Debug, Deserialize)]
struct ContentItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Default)]
struct SessionMetadata {
    first_message: Option<String>,
    message_timestamp: Option<String>,
    message_count: usize,
    total_duration: Option<f64>,
    status: String,
    cwd: Option<String>,
}

// Session file info for tracking
struct SessionFile {
    path: PathBuf,
    session_id: String,
    created_at: u64,
}

// Helper: Generate project ID from path
fn generate_project_id(path: &str) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn get_codex_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .context("Could not find home directory")?
        .join(".codex")
        .join("sessions")
        .canonicalize()
        .context("Could not find ~/.codex/sessions directory")
}

// Helper: Get timestamp from metadata
// Helper: Extract session ID from filename (rollout-{timestamp}-{session_id}.jsonl)
fn extract_session_id(filename: &str) -> Option<String> {
    filename
        .strip_prefix("rollout-")
        .and_then(|s| s.strip_suffix(".jsonl"))
        .and_then(|s| s.rsplit_once('-'))
        .map(|(_, id)| id.to_string())
}

// Helper: Recursively collect all session files from time-based directory structure
fn collect_all_session_files(base_dir: &PathBuf) -> Vec<SessionFile> {
    let mut files = Vec::new();

    // Recursively walk through year/month/day directories
    fn walk_dir(dir: &PathBuf, files: &mut Vec<SessionFile>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_dir(&path, files);
            } else if path.is_file()
                && let Some(filename) = path.file_name().and_then(|n| n.to_str())
                && filename.starts_with("rollout-")
                && filename.ends_with(".jsonl")
                && let Some(session_id) = extract_session_id(filename)
            {
                let created_at = fs::metadata(&path)
                    .ok()
                    .map(|m| metadata_timestamp(&m))
                    .unwrap_or(0);
                files.push(SessionFile {
                    path,
                    session_id,
                    created_at,
                });
            }
        }
    }

    walk_dir(base_dir, &mut files);
    files
}

fn extract_content_text(content: &ContentField) -> Option<String> {
    match content {
        ContentField::Text(text) if !should_filter_message_text(text) => Some(text.clone()),
        ContentField::Items(items) => {
            let texts: Vec<&str> = items
                .iter()
                .filter_map(|item| {
                    (item.item_type.as_deref() == Some("text"))
                        .then_some(item.text.as_ref())
                        .flatten()
                        .filter(|text| !should_filter_message_text(text))
                        .map(|s| s.as_str())
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

async fn extract_session_metadata(session_path: &PathBuf) -> SessionMetadata {
    let Ok(contents) = tokio::fs::read_to_string(session_path).await else {
        return SessionMetadata {
            status: "pending".to_string(),
            ..Default::default()
        };
    };

    let mut metadata = SessionMetadata::default();
    let mut first_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last_ts: Option<chrono::DateTime<chrono::Utc>> = None;

    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<JsonlMessage>(line) else {
            continue;
        };

        // Extract cwd from session_meta
        if entry.msg_type.as_deref() == Some("session_meta")
            && let Some(payload) = entry.payload
        {
            metadata.cwd = payload.cwd;
        }

        if entry.message.is_some() {
            metadata.message_count += 1;
        }

        if metadata.first_message.is_none()
            && let Some(msg_field) = &entry.message
        {
            let messages = match msg_field {
                MessageField::Single(m) => vec![m],
                MessageField::Multiple(ms) => ms.iter().collect(),
            };

            for msg in messages {
                if msg.role.as_deref() == Some("user")
                    && let Some(text) = msg.content.as_ref().and_then(extract_content_text)
                {
                    metadata.first_message = Some(text);
                    metadata.message_timestamp = entry.timestamp.clone();
                    break;
                }
            }
        }

        if let Some(ts_str) = &entry.timestamp
            && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts_str)
        {
            let utc = parsed.with_timezone(&chrono::Utc);
            first_ts.get_or_insert(utc);
            last_ts = Some(utc);
        }
    }

    metadata.total_duration = first_ts
        .zip(last_ts)
        .map(|(f, l)| l.signed_duration_since(f).num_milliseconds() as f64 / 1000.0);

    metadata.status = if metadata.message_count == 0 {
        "pending".to_string()
    } else if let Ok(meta) = tokio::fs::metadata(session_path).await {
        meta.modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map(|e| {
                if e.as_secs() < 60 {
                    "ongoing"
                } else {
                    "completed"
                }
            })
            .unwrap_or("completed")
            .to_string()
    } else {
        "completed".to_string()
    };

    metadata
}

pub async fn list_projects() -> Result<Vec<Project>, String> {
    let sessions_dir = get_codex_dir().map_err(|e| e.to_string())?;
    tracing::info!("Listing projects from {:?}", sessions_dir);

    if !sessions_dir.exists() {
        tracing::warn!("Sessions directory does not exist: {:?}", sessions_dir);
        return Ok(Vec::new());
    }

    let session_files = collect_all_session_files(&sessions_dir);
    tracing::info!("Found {} session files", session_files.len());

    // Group sessions by cwd (project path)
    let mut projects_map: HashMap<String, Vec<(String, u64)>> = HashMap::new();

    for file in session_files {
        let metadata = extract_session_metadata(&file.path).await;
        if let Some(cwd) = metadata.cwd {
            projects_map
                .entry(cwd.clone())
                .or_default()
                .push((file.session_id, file.created_at));
        }
    }

    let mut projects: Vec<Project> = projects_map
        .into_iter()
        .map(|(path, mut sessions_data)| {
            sessions_data.sort_by(|a, b| b.1.cmp(&a.1)); // Sort by created_at desc
            let most_recent_session = sessions_data.first().map(|(_, ts)| *ts);
            let sessions: Vec<String> = sessions_data.into_iter().map(|(id, _)| id).collect();
            let created_at = most_recent_session.unwrap_or(0);

            // Use hash of path as project ID
            let id = generate_project_id(&path);

            Project {
                id,
                path,
                sessions,
                created_at,
                most_recent_session,
            }
        })
        .collect();

    projects.sort_by(|a, b| {
        b.most_recent_session
            .unwrap_or(0)
            .cmp(&a.most_recent_session.unwrap_or(0))
    });

    tracing::info!("Found {} projects", projects.len());
    Ok(projects)
}

pub async fn load_session_by_id(session_id: String) -> Result<Vec<serde_json::Value>, String> {
    tracing::info!("Loading session history for session ID: {}", session_id);

    let sessions_dir = get_codex_dir().map_err(|e| e.to_string())?;
    if !sessions_dir.exists() {
        return Err("Sessions directory does not exist".to_string());
    }

    let clean_id = session_id.trim_end_matches(".jsonl");
    let session_files = collect_all_session_files(&sessions_dir);

    for file in session_files {
        if file.session_id == clean_id {
            tracing::info!("Found session file at: {:?}", file.path);
            let contents = tokio::fs::read_to_string(&file.path)
                .await
                .map_err(|e| format!("Failed to read session file: {}", e))?;

            return Ok(contents
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect());
        }
    }

    Err(format!(
        "Session file not found for session ID: {}",
        clean_id
    ))
}

pub async fn delete_session_by_id(session_id: String) -> Result<(), String> {
    tracing::info!("Deleting session with ID: {}", session_id);

    let sessions_dir = get_codex_dir().map_err(|e| e.to_string())?;
    if !sessions_dir.exists() {
        return Err("Sessions directory does not exist".to_string());
    }

    let clean_id = session_id.trim_end_matches(".jsonl");
    let session_files = collect_all_session_files(&sessions_dir);

    for file in session_files {
        if file.session_id == clean_id {
            tokio::fs::remove_file(&file.path)
                .await
                .map_err(|e| format!("Failed to delete session file: {}", e))?;
            tracing::info!("Removed session file at {:?}", file.path);
            return Ok(());
        }
    }

    Err(format!(
        "Session file not found for session ID: {}",
        clean_id
    ))
}

pub async fn get_all_sessions(
    limit: Option<usize>,
    offset: Option<usize>,
    project_path: Option<String>,
) -> Result<Vec<Session>, String> {
    tracing::info!(
        "Getting all sessions (limit: {:?}, offset: {:?}, project_path: {:?})",
        limit,
        offset,
        project_path
    );

    let sessions_dir = get_codex_dir().map_err(|e| e.to_string())?;
    if !sessions_dir.exists() {
        tracing::warn!("Sessions directory does not exist: {:?}", sessions_dir);
        return Ok(Vec::new());
    }

    let session_files = collect_all_session_files(&sessions_dir);
    tracing::info!("Found {} session files to process", session_files.len());

    let mut sessions = Vec::new();
    for file in session_files {
        let session_meta = extract_session_metadata(&file.path).await;

        // Filter by project_path if specified
        if let Some(ref filter_path) = project_path
            && session_meta.cwd.as_ref() != Some(filter_path)
        {
            continue;
        }

        let project_path = session_meta.cwd.clone().unwrap_or_default();
        let project_id = generate_project_id(&project_path);

        sessions.push(Session {
            id: file.session_id,
            project_id,
            project_path,
            todo_data: None, // Codex doesn't have separate todo files
            created_at: file.created_at,
            first_message: session_meta.first_message,
            message_timestamp: session_meta.message_timestamp,
            message_count: session_meta.message_count,
            status: session_meta.status,
            total_duration: session_meta.total_duration,
        });
    }

    sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    tracing::info!("Found {} sessions total", sessions.len());

    let result: Vec<_> = sessions
        .into_iter()
        .skip(offset.unwrap_or(0))
        .take(limit.unwrap_or(usize::MAX))
        .collect();

    tracing::info!("Returning {} sessions", result.len());
    Ok(result)
}

pub async fn get_working_directories() -> Result<Vec<WorkingDirectory>, String> {
    tracing::info!("Getting all project working directories");

    let sessions_dir = get_codex_dir().map_err(|e| e.to_string())?;
    if !sessions_dir.exists() {
        tracing::warn!("Sessions directory does not exist: {:?}", sessions_dir);
        return Ok(Vec::new());
    }

    let session_files = collect_all_session_files(&sessions_dir);

    // Group by cwd
    let mut cwd_map: HashMap<String, (Vec<String>, u64)> = HashMap::new();
    for file in session_files {
        let metadata = extract_session_metadata(&file.path).await;
        if let Some(cwd) = metadata.cwd {
            let entry = cwd_map.entry(cwd).or_insert_with(|| (Vec::new(), 0));
            entry.0.push(file.session_id);
            entry.1 = entry.1.max(file.created_at);
        }
    }

    let mut directories: Vec<WorkingDirectory> = cwd_map
        .into_iter()
        .map(|(path, (sessions, last_timestamp))| {
            let components: Vec<&str> = path.split('/').collect();
            let short_name = if components.len() >= 2 {
                format!(
                    "{}/{}",
                    components[components.len() - 2],
                    components[components.len() - 1]
                )
            } else {
                path.clone()
            };

            let last_date = chrono::DateTime::<chrono::Utc>::from(
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(last_timestamp),
            )
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

            WorkingDirectory {
                path,
                short_name,
                last_date,
                conversation_count: sessions.len(),
                AgentName: AGENT_NAME.to_string(),
            }
        })
        .collect();

    directories.sort_by(|a, b| b.last_date.cmp(&a.last_date));
    tracing::info!("Found {} working directories", directories.len());
    Ok(directories)
}
