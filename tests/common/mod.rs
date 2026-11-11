//! Shared by the CLI tests (`mod common;`).

/// The caller's PATH without any pmux tmux guard directory, so a test's pmux
/// finds the real tmux the same way whether or not `cargo test` runs inside a
/// pmux pane, where the caller's own guard is first on PATH.
pub fn path() -> String {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty() && !is_guard(&std::path::Path::new(dir).join("tmux")))
        .collect::<Vec<_>>()
        .join(":")
}

fn is_guard(tmux: &std::path::Path) -> bool {
    std::fs::read(tmux).is_ok_and(|bytes| {
        let head = &bytes[..bytes.len().min(1024)];
        String::from_utf8_lossy(head).contains("\n# pmux's tmux guard")
    })
}
