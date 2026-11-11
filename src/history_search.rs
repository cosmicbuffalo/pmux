//! Fuzzy search over the history catalog.
//!
//! `pmux history --query` ranks past sessions through [`fields`] and
//! [`rank`]: a session's searchable text is its title, agent, id, branch and
//! displayed directory, joined with spaces, and nucleo scores the query
//! against that one string. Matched positions are
//! reported per field so a client (the iOS app) can light them up as the
//! picker would, without reimplementing the matcher.

use crate::history::HistoryEntry;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::Serialize;

/// Where a query matched one field of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldMatch {
    /// `title`, `agent`, `id`, `branch` or `cwd`.
    pub field: &'static str,
    /// The field's text as searched (`cwd` is shown with `~` for home).
    pub text: String,
    /// Positions of the matched characters in `text`, as grapheme cluster
    /// offsets (Swift `Character` offsets), ascending.
    pub indices: Vec<u32>,
}

/// How well a query matched a session, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryMatch {
    pub score: u32,
    /// Only the fields with at least one matched character.
    pub fields: Vec<FieldMatch>,
}

/// One ranked session: the entry with its match, flattened in JSON.
#[derive(Debug, Clone, Serialize)]
pub struct RankedEntry {
    #[serde(flatten)]
    pub entry: HistoryEntry,
    #[serde(rename = "match")]
    pub matched: HistoryMatch,
}

/// The searchable fields of `entry`, in haystack order.
pub(crate) fn fields(entry: &HistoryEntry) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();
    if let Some(title) = &entry.title {
        fields.push(("title", title.clone()));
    }
    fields.push(("agent", entry.agent.clone()));
    fields.push(("id", entry.session_id.clone()));
    if let Some(branch) = &entry.git_branch {
        fields.push(("branch", branch.clone()));
    }
    if let Some(cwd) = &entry.cwd {
        fields.push(("cwd", crate::display_path(cwd)));
    }
    fields
}

/// The query pattern for `query`: case-insensitive, smart normalization.
pub(crate) fn pattern(query: &str) -> Pattern {
    Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart)
}

/// Score and locate `query` in `entry`, or `None` when it does not match.
fn match_entry(
    pattern: &Pattern,
    matcher: &mut Matcher,
    entry: &HistoryEntry,
) -> Option<HistoryMatch> {
    let fields = fields(entry);
    let joined = fields
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut buf = Vec::new();
    let mut indices = Vec::new();
    let score = pattern.indices(Utf32Str::new(&joined, &mut buf), matcher, &mut indices)?;
    indices.sort_unstable();
    indices.dedup();

    // Split the joined positions back into fields; a match on a separating
    // space belongs to none.
    let mut matched = Vec::new();
    let mut start = 0u32;
    let mut field_buf = Vec::new();
    for (field, text) in fields {
        let len = Utf32Str::new(&text, &mut field_buf).len() as u32;
        let end = start + len;
        let local: Vec<u32> = indices
            .iter()
            .filter(|&&index| index >= start && index < end)
            .map(|index| index - start)
            .collect();
        if !local.is_empty() {
            matched.push(FieldMatch {
                field,
                text,
                indices: local,
            });
        }
        start = end + 1;
    }
    Some(HistoryMatch {
        score,
        fields: matched,
    })
}

/// The entries matching `query`, best first; ties keep the given order
/// (newest first from the catalog).
pub(crate) fn rank(entries: Vec<HistoryEntry>, query: &str) -> Vec<RankedEntry> {
    let pattern = pattern(query);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut ranked: Vec<(usize, RankedEntry)> = entries
        .into_iter()
        .enumerate()
        .filter_map(|(order, entry)| {
            let matched = match_entry(&pattern, &mut matcher, &entry)?;
            Some((order, RankedEntry { entry, matched }))
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .matched
            .score
            .cmp(&left.1.matched.score)
            .then(left.0.cmp(&right.0))
    });
    ranked.into_iter().map(|(_, ranked)| ranked).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str, cwd: &str) -> HistoryEntry {
        HistoryEntry {
            agent: "claude".to_string(),
            session_id: id.to_string(),
            title: Some(title.to_string()),
            title_source: None,
            first_prompt: None,
            cwd: Some(cwd.to_string()),
            cwd_exists: true,
            git_branch: Some("main".to_string()),
            started_epoch: None,
            last_activity_epoch: 0,
            transcript: String::new(),
            live: false,
            pmux_process: None,
            resume_command: None,
        }
    }

    #[test]
    fn ranks_best_first_and_drops_non_matches() {
        let ranked = rank(
            vec![
                entry("aaaa", "Fix the login flow", "/srv/web"),
                entry("bbbb", "Refactor billing", "/srv/pay"),
                entry("cccc", "Bill splitting idea", "/srv/x"),
            ],
            "billing",
        );
        let ids: Vec<&str> = ranked.iter().map(|r| r.entry.session_id.as_str()).collect();
        assert_eq!(ids.first(), Some(&"bbbb"));
        assert!(!ids.contains(&"aaaa"));
    }

    #[test]
    fn indices_are_reported_per_field() {
        let ranked = rank(
            vec![entry("abcd1234", "Fix login", "/srv/web")],
            "login web",
        );
        let fields = &ranked[0].matched.fields;
        let title = fields.iter().find(|f| f.field == "title").unwrap();
        assert_eq!(title.text, "Fix login");
        assert_eq!(title.indices, vec![4, 5, 6, 7, 8]);
        let cwd = fields.iter().find(|f| f.field == "cwd").unwrap();
        assert_eq!(cwd.indices, vec![5, 6, 7]);
        assert!(fields.iter().all(|f| f.field != "agent"));
    }

    #[test]
    fn non_ascii_indices_count_characters() {
        let ranked = rank(vec![entry("abcd1234", "日本語 deploy", "/srv")], "deploy");
        let title = &ranked[0].matched.fields[0];
        assert_eq!(title.field, "title");
        assert_eq!(title.indices, vec![4, 5, 6, 7, 8, 9]);
    }
}
