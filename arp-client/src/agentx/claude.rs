use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::SystemTime;

pub use crate::agentx::types::{Project, Session, WorkingDirectory};
use crate::agentx::utils::{metadata_timestamp, should_filter_message_text};

const AGENT_NAME: &str = "Claude Code";

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MessageField {
    Single(MessageContent),
    Multiple(Vec<MessageContent>),
}

#[derive(Debug, Deserialize)]
struct JsonlMessage {
    message: Option<MessageField>,
    timestamp: Option<String>,
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
}

fn get_claude_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .context("Could not find home directory")?
        .join(".claude")
        .canonicalize()
        .context("Could not find ~/.claude directory")
}

// Helper: Collect jsonl sessions with timestamps
fn collect_sessions(dir: &PathBuf) -> Option<(Vec<String>, Option<u64>)> {
    Some(
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.is_file() && p.extension()?.to_str()? == "jsonl").then(|| {
                    let id = p.file_stem()?.to_str()?.to_string();
                    let ts = fs::metadata(&p).ok().map(|m| metadata_timestamp(&m))?;
                    Some((id, ts))
                })?
            })
            .fold((Vec::new(), None), |(mut ids, max_ts), (id, ts)| {
                ids.push(id);
                (ids, Some(max_ts.map_or(ts, |m: u64| m.max(ts))))
            }),
    )
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

pub fn get_project_path_from_sessions(project_dir: &PathBuf) -> Result<String, String> {
    fs::read_dir(project_dir)
        .map_err(|e| format!("Failed to read project directory: {}", e))?
        .flatten()
        .find_map(|entry| {
            let path = entry.path();
            (path.is_file() && path.extension()?.to_str()? == "jsonl")
                .then(|| {
                    BufReader::new(fs::File::open(&path).ok()?)
                        .lines()
                        .take(10)
                        .flatten()
                        .find_map(|line| {
                            serde_json::from_str::<serde_json::Value>(&line)
                                .ok()?
                                .get("cwd")?
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                        })
                })
                .flatten()
        })
        .ok_or_else(|| "Could not determine project path from session files".to_string())
}

fn decode_project_path(encoded: &str) -> String {
    encoded.replace('-', "/")
}

async fn extract_session_metadata(jsonl_path: &PathBuf) -> SessionMetadata {
    let Ok(file) = fs::File::open(jsonl_path) else {
        return SessionMetadata {
            status: "pending".to_string(),
            ..Default::default()
        };
    };

    let mut metadata = SessionMetadata::default();
    let mut first_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last_ts: Option<chrono::DateTime<chrono::Utc>> = None;

    for line in BufReader::new(file).lines().flatten() {
        let Ok(entry) = serde_json::from_str::<JsonlMessage>(&line) else {
            continue;
        };

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
    } else if let Ok(meta) = fs::metadata(jsonl_path) {
        meta.modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map(|e| {
                if e.as_secs() < 3 {
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
    let projects_dir = get_claude_dir()
        .map_err(|e| e.to_string())?
        .join("projects");
    tracing::info!("Listing projects from {:?}", projects_dir);

    if !projects_dir.exists() {
        tracing::warn!("Projects directory does not exist: {:?}", projects_dir);
        return Ok(Vec::new());
    }

    let mut projects: Vec<Project> = fs::read_dir(&projects_dir)
        .map_err(|e| format!("Failed to read projects directory: {}", e))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }

            let dir_name = path.file_name()?.to_str()?.to_string();
            let metadata = fs::metadata(&path).ok()?;
            let created_at = metadata_timestamp(&metadata);
            let project_path = get_project_path_from_sessions(&path)
                .unwrap_or_else(|_| decode_project_path(&dir_name));
            let (sessions, most_recent_session) = collect_sessions(&path)?;

            Some(Project {
                id: dir_name,
                path: project_path,
                sessions,
                created_at,
                most_recent_session,
            })
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

    tracing::info!("Found {} projects", projects.len());
    Ok(projects)
}
pub async fn load_session_by_id(session_id: String) -> Result<Vec<serde_json::Value>, String> {
    tracing::info!("Loading session history for session ID: {}", session_id);

    let projects_dir = get_claude_dir()
        .map_err(|e| e.to_string())?
        .join("projects");
    if !projects_dir.exists() {
        return Err("Projects directory does not exist".to_string());
    }

    let clean_id = session_id.trim_end_matches(".jsonl");

    fs::read_dir(&projects_dir)
        .map_err(|e| format!("Failed to read projects directory: {}", e))?
        .flatten()
        .find_map(|entry| {
            let session_path = entry.path().join(format!("{}.jsonl", clean_id));
            session_path
                .exists()
                .then(|| {
                    tracing::info!("Found session file at: {:?}", session_path);
                    BufReader::new(fs::File::open(&session_path).ok()?)
                        .lines()
                        .flatten()
                        .filter_map(|line| serde_json::from_str(&line).ok())
                        .collect()
                })
                .flatten()
        })
        .ok_or_else(|| format!("Session file not found for session ID: {}", clean_id))
}

pub async fn delete_session_by_id(session_id: String) -> Result<(), String> {
    tracing::info!("Deleting session with ID: {}", session_id);

    let claude_dir = get_claude_dir().map_err(|e| e.to_string())?;
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return Err("Projects directory does not exist".to_string());
    }

    let clean_id = session_id.trim_end_matches(".jsonl");
    let session_file = format!("{}.jsonl", clean_id);

    let session_path = fs::read_dir(&projects_dir)
        .map_err(|e| format!("Failed to read projects directory: {}", e))?
        .flatten()
        .find_map(|e| {
            let p = e.path().join(&session_file);
            p.exists().then_some(p)
        })
        .ok_or_else(|| format!("Session file not found for session ID: {}", clean_id))?;

    fs::remove_file(&session_path).map_err(|e| format!("Failed to delete session file: {}", e))?;
    tracing::info!("Removed session file at {:?}", session_path);

    let todo_path = claude_dir.join("todos").join(format!("{}.json", clean_id));
    if todo_path.exists() {
        if let Err(e) = fs::remove_file(&todo_path) {
            tracing::warn!("Failed to delete session todo file {:?}: {}", todo_path, e);
        } else {
            tracing::info!("Removed session todo file at {:?}", todo_path);
        }
    }

    Ok(())
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

    let claude_dir = get_claude_dir().map_err(|e| e.to_string())?;
    let projects_dir = claude_dir.join("projects");
    let todos_dir = claude_dir.join("todos");

    if !projects_dir.exists() {
        tracing::warn!("Projects directory does not exist: {:?}", projects_dir);
        return Ok(Vec::new());
    }

    let project_infos: Vec<_> = fs::read_dir(&projects_dir)
        .map_err(|e| format!("Failed to read projects directory: {}", e))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }

            let id = path.file_name()?.to_str()?.to_string();
            let real_path =
                get_project_path_from_sessions(&path).unwrap_or_else(|_| decode_project_path(&id));

            if let Some(ref filter) = project_path
                && &real_path != filter
            {
                return None;
            }

            Some((path, id, real_path))
        })
        .collect();

    tracing::info!("Found {} projects to process", project_infos.len());

    let mut sessions = Vec::new();
    for (proj_path, proj_id, proj_real_path) in project_infos {
        let Ok(entries) = fs::read_dir(&proj_path) else {
            continue;
        };

        for entry in entries.flatten() {
            let session_path = entry.path();
            if !session_path.is_file()
                || session_path.extension().and_then(|s| s.to_str()) != Some("jsonl")
            {
                continue;
            }

            let Some(session_id) = session_path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(meta) = fs::metadata(&session_path) else {
                continue;
            };
            let created_at = metadata_timestamp(&meta);

            let session_meta = extract_session_metadata(&session_path).await;
            let todo_data = todos_dir
                .join(format!("{}.json", session_id))
                .exists()
                .then(|| fs::read_to_string(todos_dir.join(format!("{}.json", session_id))).ok())
                .flatten()
                .and_then(|c| serde_json::from_str(&c).ok());

            sessions.push(Session {
                id: session_id.to_string(),
                project_id: proj_id.clone(),
                project_path: proj_real_path.clone(),
                todo_data,
                created_at,
                first_message: session_meta.first_message,
                message_timestamp: session_meta.message_timestamp,
                message_count: session_meta.message_count,
                status: session_meta.status,
                total_duration: session_meta.total_duration,
            });
        }
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

    let projects_dir = get_claude_dir()
        .map_err(|e| e.to_string())?
        .join("projects");
    if !projects_dir.exists() {
        tracing::warn!("Projects directory does not exist: {:?}", projects_dir);
        return Ok(Vec::new());
    }

    let mut directories: Vec<WorkingDirectory> = fs::read_dir(&projects_dir)
        .map_err(|e| format!("Failed to read projects directory: {}", e))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }

            let real_path = get_project_path_from_sessions(&path).ok()?;

            let components: Vec<&str> = real_path.split('/').collect();
            let short_name = if components.len() >= 2 {
                format!(
                    "{}/{}",
                    components[components.len() - 2],
                    components[components.len() - 1]
                )
            } else {
                real_path.clone()
            };

            let (sessions, most_recent) = collect_sessions(&path)?;
            let timestamp = most_recent.unwrap_or_else(|| {
                fs::metadata(&path)
                    .ok()
                    .map(|m| metadata_timestamp(&m))
                    .unwrap_or(0)
            });

            let last_date = chrono::DateTime::<chrono::Utc>::from(
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp),
            )
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

            Some(WorkingDirectory {
                path: real_path,
                short_name,
                last_date,
                conversation_count: sessions.len(),
                AgentName: AGENT_NAME.to_string(),
            })
        })
        .collect();

    directories.sort_by(|a, b| b.last_date.cmp(&a.last_date));
    tracing::info!("Found {} working directories", directories.len());
    Ok(directories)
}
