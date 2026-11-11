//! Optional Dredge-backed full-text search for the attach picker.
//!
//! Dredge is deliberately an external, optional dependency. This adapter owns
//! its versioned JSON boundary and runs every command off the UI thread. A
//! missing binary or failed/slow query never prevents the picker from using
//! its built-in fuzzy filter over live processes.

use serde::Deserialize;
use std::io;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

const API_VERSION: u32 = 1;
const SEARCH_LIMIT: usize = 50;
const INDEX_TIMEOUT: Duration = Duration::from_secs(60);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SearchHit {
    pub provider: String,
    pub session_id: String,
    pub score: f32,
    pub match_source: Option<String>,
    pub snippet: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum SearchUpdate {
    /// The executable was not found. Absence is normal for an optional tool.
    Absent,
    /// The index refresh finished and typeahead queries can use Dredge.
    Ready,
    /// Results belong to `query`; callers must discard them after input moves on.
    Results { query: String, hits: Vec<SearchHit> },
    /// Dredge exists but failed. Kept terse because it is shown in one line.
    Failed(String),
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    api_version: u32,
    results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    provider: String,
    session_id: String,
    score: Option<f32>,
    match_source: Option<String>,
    snippet: Option<String>,
}

#[derive(Debug)]
enum CommandError {
    Absent,
    Failed(String),
}

/// Start one serialized Dredge worker. It refreshes the index once, then
/// coalesces queued queries before spawning a search so fast typing does not
/// build an obsolete process backlog.
pub(crate) fn spawn_search_worker(queries: Receiver<String>, updates: Sender<SearchUpdate>) {
    crate::render::spawn_worker(
        "pmux-dredge-search",
        move || {
            match run_dredge(&["index", "--json"], INDEX_TIMEOUT) {
                Ok(_) => {
                    if updates.send(SearchUpdate::Ready).is_err() {
                        return;
                    }
                }
                Err(CommandError::Absent) => {
                    let _ = updates.send(SearchUpdate::Absent);
                    return;
                }
                Err(CommandError::Failed(message)) => {
                    let _ = updates.send(SearchUpdate::Failed(message));
                    return;
                }
            }

            while let Ok(mut query) = queries.recv() {
                while let Ok(newer) = queries.try_recv() {
                    query = newer;
                }
                if query.trim().is_empty() {
                    continue;
                }
                let limit = SEARCH_LIMIT.to_string();
                let output = run_dredge(
                    &[
                        "search",
                        "--refresh",
                        "never",
                        "--json",
                        "--limit",
                        &limit,
                        &query,
                    ],
                    SEARCH_TIMEOUT,
                );
                let update = match output.and_then(|bytes| decode_search(&bytes)) {
                    Ok(hits) => SearchUpdate::Results { query, hits },
                    Err(CommandError::Absent) => SearchUpdate::Absent,
                    Err(CommandError::Failed(message)) => SearchUpdate::Failed(message),
                };
                let terminal = matches!(update, SearchUpdate::Absent | SearchUpdate::Failed(_));
                if updates.send(update).is_err() || terminal {
                    return;
                }
            }
        },
        || {},
    );
}

fn dredge_program() -> std::ffi::OsString {
    std::env::var_os("PMUX_DREDGE_BIN").unwrap_or_else(|| "dredge".into())
}

fn run_dredge(args: &[&str], timeout: Duration) -> Result<Vec<u8>, CommandError> {
    let output =
        crate::util::output_with_timeout(Command::new(dredge_program()).args(args), timeout)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    CommandError::Absent
                } else if error.kind() == io::ErrorKind::TimedOut {
                    CommandError::Failed("Dredge timed out".to_string())
                } else {
                    CommandError::Failed(format!("Dredge could not start: {error}"))
                }
            })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(CommandError::Failed(format!(
            "Dredge exited with {}",
            output.status
        )))
    }
}

fn decode_search(bytes: &[u8]) -> Result<Vec<SearchHit>, CommandError> {
    let envelope: SearchEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| CommandError::Failed(format!("Invalid Dredge JSON: {error}")))?;
    if envelope.api_version != API_VERSION {
        return Err(CommandError::Failed(format!(
            "Unsupported Dredge API version {}",
            envelope.api_version
        )));
    }
    Ok(envelope
        .results
        .into_iter()
        .filter_map(|result| {
            let score = result.score?;
            score.is_finite().then_some(SearchHit {
                provider: result.provider,
                session_id: result.session_id,
                score: score.clamp(0.0, 1.0),
                match_source: result.match_source,
                snippet: result.snippet,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_api_one_and_clamps_scores() {
        let hits = decode_search(
            br#"{"api_version":1,"results":[
                {"provider":"claude","session_id":"a","score":0.75,
                 "match_source":"conversation","snippet":"the matching text"},
                {"provider":"codex","session_id":"b","score":2.0,
                 "match_source":"title","snippet":"Matching title"},
                {"provider":"codex","session_id":"c","score":null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            hits,
            vec![
                SearchHit {
                    provider: "claude".to_string(),
                    session_id: "a".to_string(),
                    score: 0.75,
                    match_source: Some("conversation".to_string()),
                    snippet: Some("the matching text".to_string()),
                },
                SearchHit {
                    provider: "codex".to_string(),
                    session_id: "b".to_string(),
                    score: 1.0,
                    match_source: Some("title".to_string()),
                    snippet: Some("Matching title".to_string()),
                }
            ]
        );
    }

    #[test]
    fn rejects_unknown_api_versions_and_bad_json() {
        assert!(matches!(
            decode_search(br#"{"api_version":2,"results":[]}"#),
            Err(CommandError::Failed(message)) if message.contains("version 2")
        ));
        assert!(matches!(
            decode_search(b"not json"),
            Err(CommandError::Failed(message)) if message.contains("Invalid Dredge JSON")
        ));
    }
}
