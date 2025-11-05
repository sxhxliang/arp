pub mod claude;
// pub mod claude_routes;
pub mod codex;
// pub mod codex_routes;
pub mod gemini;
// pub mod gemini_routes;
// pub mod routes_common;
pub mod types;
pub mod utils;

use std::collections::HashMap;
use types::WorkingDirectory;

/// Get all working directories from Claude, Codex, and Gemini, merged and deduplicated by path
pub async fn get_all_working_directories() -> Result<Vec<WorkingDirectory>, String> {
    tracing::info!("Getting all working directories from all agents");

    // Collect results from all agents concurrently
    let (claude_result, codex_result, gemini_result) = tokio::join!(
        claude::get_working_directories(),
        codex::get_working_directories(),
        gemini::get_working_directories()
    );

    // Combine all results
    let mut all_dirs = Vec::new();

    if let Ok(dirs) = claude_result {
        all_dirs.extend(dirs);
    }

    if let Ok(dirs) = codex_result {
        all_dirs.extend(dirs);
    }

    if let Ok(dirs) = gemini_result {
        all_dirs.extend(dirs);
    }

    // Merge duplicates by path
    let mut merged: HashMap<String, WorkingDirectory> = HashMap::new();

    for dir in all_dirs {
        let path = dir.path.clone();

        merged
            .entry(path.clone())
            .and_modify(|existing| {
                // Merge conversation counts
                existing.conversation_count += dir.conversation_count;

                // Use the most recent last_date
                if dir.last_date > existing.last_date {
                    existing.last_date = dir.last_date.clone();
                }

                // Merge agent names
                if !existing.AgentName.contains(&dir.AgentName) {
                    existing.AgentName = format!("{}, {}", existing.AgentName, dir.AgentName);
                }
            })
            .or_insert(dir);
    }

    // Convert back to vec and sort by last_date
    let mut result: Vec<WorkingDirectory> = merged.into_values().collect();
    result.sort_by(|a, b| b.last_date.cmp(&a.last_date));

    tracing::info!("Found {} unique working directories", result.len());
    Ok(result)
}
