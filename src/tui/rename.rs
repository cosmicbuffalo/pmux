//! The rename form behind `prefix ,` and the sidebar's rename action, built
//! on the shared floating prompt.

use super::*;

/// A rename prompt for `session_name`, starting with its display name.
/// Generated names start empty rather than exposing the internal name.
pub(super) fn rename_prompt(session_name: &str) -> Prompt {
    let title = crate::display_session_name(session_name)
        .map(|name| format!("Rename [{name}]"))
        .unwrap_or_else(|| "Rename process".to_string());
    Prompt::text(
        title,
        "Rename",
        crate::display_session_name(session_name).unwrap_or_default(),
    )
}

pub(super) fn submit_rename(session_name: &str, value: &str) -> std::result::Result<(), String> {
    let name = value.trim();
    if name.is_empty() {
        return Err("Enter a session name before renaming.".to_string());
    }
    crate::rename_session(session_name, name).map_err(|err| format!("Rename failed: {err}"))
}

/// Run the rename form used by the regular `prefix + ,` tmux binding.
pub fn run_rename_session_ui(session_name: &str) -> Result<()> {
    run_prompt(rename_prompt(session_name), |value| {
        submit_rename(session_name, value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_rejects_an_invalid_name_before_calling_tmux() {
        // A leading dash is rejected before any tmux call is made.
        assert_eq!(
            submit_rename("original", "-name"),
            Err(
                "Rename failed: session name '-name' cannot start with '-' (conflicts with tmux option parsing)"
                    .to_string()
            )
        );
        assert_eq!(
            submit_rename("original", "   "),
            Err("Enter a session name before renaming.".to_string())
        );
    }
}
