use crate::handlers::HandlerState;
use crate::router::HandlerContext;
use anyhow::Result;
use chrono::{DateTime, Utc};
use common::http::{HttpResponse, json_error};
use serde_json::json;
use std::borrow::Cow;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncReadExt;

const MAX_FILE_BYTES: usize = 1_048_576;

pub async fn handle_filesystem(ctx: HandlerContext, state: HandlerState) -> Result<HttpResponse> {
    let HandlerContext {
        request,
        mut stream,
        proxy_conn_id: _,
        mut path_params,
    } = ctx;

    if !state.config.enable_fs {
        let _ = json_error(403, "Filesystem browsing API is disabled")
            .send(&mut stream)
            .await;
        return Ok(HttpResponse::ok());
    }

    let mut session_id_for_response: Option<String> = None;

    let base_path_candidate = if let Some(session_id) = path_params.get("session_id").cloned() {
        session_id_for_response = Some(session_id.clone());
        return Ok(HttpResponse::ok());
        // let session = match state.session_manager.get_session(&session_id).await {
        //     Some(session) => session,
        //     None => {
        //         let _ = json_error(404, "Session not found").send(&mut stream).await;
        //         return Ok(HttpResponse::ok());
        //     }
        // };

        // match session.get_project_path().await {
        //     Some(path) => path,
        //     None => {
        //         let _ = json_error(404, "Project path unavailable for this session")
        //             .send(&mut stream)
        //             .await;
        //         return Ok(HttpResponse::ok());
        //     }
        // }
    } else {
        let project_path_raw = match request.query_param("project_path") {
            Some(value) if !value.trim().is_empty() => value.clone(),
            _ => {
                let _ = json_error(
                    400,
                    "project_path query parameter is required when session_id is not provided",
                )
                .send(&mut stream)
                .await;
                return Ok(HttpResponse::ok());
            }
        };

        let decoded = match urlencoding::decode(project_path_raw.as_str()) {
            Ok(path) => path.into_owned(),
            Err(e) => {
                let _ = json_error(
                    400,
                    format!("Failed to decode project_path parameter: {}", e),
                )
                .send(&mut stream)
                .await;
                return Ok(HttpResponse::ok());
            }
        };

        PathBuf::from(decoded)
    };

    let canonical_base = match fs::canonicalize(&base_path_candidate).await {
        Ok(path) => path,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let _ = json_error(
                404,
                "Requested project_path does not exist or is not accessible",
            )
            .send(&mut stream)
            .await;
            return Ok(HttpResponse::ok());
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let _ = json_error(403, "Permission denied accessing project_path")
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
        Err(e) => {
            let _ = json_error(500, format!("Failed to resolve project_path: {}", e))
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
    };

    let base_metadata = match fs::metadata(&canonical_base).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let _ = json_error(
                404,
                "Requested project_path does not exist or is not accessible",
            )
            .send(&mut stream)
            .await;
            return Ok(HttpResponse::ok());
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let _ = json_error(403, "Permission denied accessing project_path")
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
        Err(e) => {
            let _ = json_error(
                500,
                format!("Failed to access project_path metadata: {}", e),
            )
            .send(&mut stream)
            .await;
            return Ok(HttpResponse::ok());
        }
    };

    if !base_metadata.is_dir() {
        let _ = json_error(400, "project_path must reference a directory")
            .send(&mut stream)
            .await;
        return Ok(HttpResponse::ok());
    }

    let raw_path = path_params
        .remove("path")
        .or_else(|| request.query_param("path").cloned())
        .unwrap_or_default();

    let decoded_path = match urlencoding::decode(&raw_path) {
        Ok(value) => value.into_owned(),
        Err(e) => {
            let _ = json_error(400, format!("Failed to decode path parameter: {}", e))
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
    };

    let resolved_path = match resolve_path(&canonical_base, decoded_path.as_str()) {
        Ok(path) => path,
        Err(message) => {
            let _ = json_error(400, message).send(&mut stream).await;
            return Ok(HttpResponse::ok());
        }
    };

    let metadata = match fs::metadata(&resolved_path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let _ = json_error(
                404,
                format!("Path not found: {}", decoded_path.trim_start_matches('/')),
            )
            .send(&mut stream)
            .await;
            return Ok(HttpResponse::ok());
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let _ = json_error(403, "Permission denied accessing requested path")
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
        Err(e) => {
            let _ = json_error(500, format!("Failed to access path: {}", e))
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
    };

    let canonical_target = match fs::canonicalize(&resolved_path).await {
        Ok(path) => path,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let _ = json_error(403, "Permission denied accessing requested path")
                .send(&mut stream)
                .await;
            return Ok(HttpResponse::ok());
        }
        Err(_) => resolved_path.clone(),
    };

    if !canonical_target.starts_with(&canonical_base) {
        let _ = json_error(
            403,
            "Access denied: requested path escapes the project directory",
        )
        .send(&mut stream)
        .await;
        return Ok(HttpResponse::ok());
    }

    let project_root_display = canonical_base.to_string_lossy().replace('\\', "/");

    if metadata.is_dir() {
        let entries = match list_directory(&canonical_base, &canonical_target).await {
            Ok(entries) => entries,
            Err(message) => {
                let _ = json_error(500, message).send(&mut stream).await;
                return Ok(HttpResponse::ok());
            }
        };

        let body = json!({
            "type": "directory",
            "session_id": session_id_for_response,
            "project_path": project_root_display,
            "path": decoded_path,
            "entries": entries,
        });

        HttpResponse::ok().json(&body).send(&mut stream).await?;
        return Ok(HttpResponse::ok());
    }

    if metadata.is_file() {
        let (content, encoding, truncated, bytes_read) =
            match read_file_content(&canonical_target).await {
                Ok(result) => result,
                Err(message) => {
                    let _ = json_error(500, message).send(&mut stream).await;
                    return Ok(HttpResponse::ok());
                }
            };

        let body = json!({
            "type": "file",
            "session_id": session_id_for_response,
            "project_path": project_root_display,
            "path": decoded_path,
            "size": metadata.len(),
            "bytes": bytes_read,
            "encoding": encoding,
            "truncated": truncated,
            "content": content,
        });

        HttpResponse::ok().json(&body).send(&mut stream).await?;
        return Ok(HttpResponse::ok());
    }

    let _ = json_error(400, "Requested path is neither file nor directory")
        .send(&mut stream)
        .await;
    Ok(HttpResponse::ok())
}

fn resolve_path(base: &Path, relative: &str) -> Result<PathBuf, String> {
    let mut resolved = base.to_path_buf();

    if relative.is_empty() {
        return Ok(resolved);
    }

    let relative_path = Path::new(relative);
    for component in relative_path.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("Path traversal outside the project directory is not allowed".into());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("Absolute paths are not allowed".into());
            }
        }
    }

    Ok(resolved)
}

async fn list_directory(base: &Path, target: &Path) -> Result<Vec<serde_json::Value>, String> {
    let mut entries = fs::read_dir(target)
        .await
        .map_err(|e| format!("Failed to read directory: {}", e))?;

    let mut result = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("Failed to iterate directory: {}", e))?
    {
        let file_name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };

        if file_name == "." || file_name == ".." {
            continue;
        }

        let entry_path = entry.path();
        let metadata = entry
            .metadata()
            .await
            .map_err(|e| format!("Failed to read metadata: {}", e))?;

        let relative = entry_path
            .strip_prefix(base)
            .unwrap_or(entry_path.as_path())
            .to_string_lossy()
            .replace('\\', "/");

        let modified = metadata
            .modified()
            .ok()
            .map(|ts| DateTime::<Utc>::from(ts).to_rfc3339());

        result.push(json!({
            "name": file_name,
            "path": relative,
            "is_dir": metadata.is_dir(),
            "size": if metadata.is_file() { Some(metadata.len()) } else { None },
            "modified": modified,
        }));
    }

    result.sort_by(|a, b| {
        let a_is_dir = a["is_dir"].as_bool().unwrap_or(false);
        let b_is_dir = b["is_dir"].as_bool().unwrap_or(false);
        a_is_dir
            .cmp(&b_is_dir)
            .reverse()
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });

    Ok(result)
}

async fn read_file_content(path: &Path) -> Result<(String, String, bool, usize), String> {
    let file = fs::File::open(path)
        .await
        .map_err(|e| format!("Failed to read file: {}", e))?;

    let mut reader = file.take((MAX_FILE_BYTES + 1) as u64);
    let mut buffer = Vec::new();
    reader
        .read_to_end(&mut buffer)
        .await
        .map_err(|e| format!("Failed to read file content: {}", e))?;

    let truncated = buffer.len() > MAX_FILE_BYTES;
    if truncated {
        buffer.truncate(MAX_FILE_BYTES);
    }

    let bytes_read = buffer.len();
    let content_cow = String::from_utf8_lossy(&buffer);
    let (content, encoding) = match content_cow {
        Cow::Borrowed(s) => (s.to_string(), "utf-8".to_string()),
        Cow::Owned(s) => (s, "utf-8-lossy".to_string()),
    };

    Ok((content, encoding, truncated, bytes_read))
}

#[cfg(test)]
mod tests {
    use super::resolve_path;
    use std::path::PathBuf;

    #[test]
    fn resolve_path_rejects_parent_dir() {
        let base = PathBuf::from("/workspace/project");
        let result = resolve_path(&base, "../secret");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_path_rejects_absolute() {
        let base = PathBuf::from("/workspace/project");
        let result = resolve_path(&base, "/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_path_joins_relative() {
        let base = PathBuf::from("/workspace/project");
        let result = resolve_path(&base, "src/lib.rs").unwrap();
        assert_eq!(result, PathBuf::from("/workspace/project/src/lib.rs"));
    }
}
