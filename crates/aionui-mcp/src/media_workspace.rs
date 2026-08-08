//! Telling the built-in media MCP which directory a session works in.
//!
//! The media MCP writes generated images and videos into a directory it is
//! given. It cannot work that directory out for itself: its child process
//! inherits aioncore's cwd rather than the session's (stdio MCP servers are
//! spawned without `current_dir`), and the tool call carries no conversation
//! identity, so there is nothing in the request to resolve either. Left alone
//! it falls back to the application data folder.
//!
//! The misplacement costs more than a wrong path. A conversation renders media
//! result cards by matching a job's workspace against its own, so a job written
//! somewhere else silently loses its thumbnail, its open-folder and regenerate
//! actions, and its cost line — the agent reports a bare path and the user sees
//! nothing. One environment variable restores all of it.
//!
//! Scoped deliberately to this one server. Handing every session MCP server the
//! workspace path would tell arbitrary user-configured processes where the
//! user's files live, which is a much larger decision than "the media tool
//! should save next to the conversation".

/// Name of the bundled media-generation MCP server.
///
/// Still carries the upstream `aionui-` prefix: existing installations
/// recognise the server by this name, so renaming it is a product decision with
/// a migration attached, not a branding sweep.
pub const BUILTIN_MEDIA_MCP_NAME: &str = "aionui-image-generation";

/// Environment variable the media MCP reads to decide where generated files go.
pub const MEDIA_WORKSPACE_ENV: &str = "AIONUI_MEDIA_WORKSPACE_DIR";

/// The workspace variable to add for `server_name`, if it wants one.
///
/// Returns `None` for every other server, and for an empty workspace — an empty
/// value would override the tool's own fallback with nothing, which is worse
/// than leaving it to fall back.
pub fn media_workspace_env(server_name: &str, workspace: &str) -> Option<(String, String)> {
    if server_name != BUILTIN_MEDIA_MCP_NAME {
        return None;
    }
    let workspace = workspace.trim();
    if workspace.is_empty() {
        return None;
    }
    Some((MEDIA_WORKSPACE_ENV.to_string(), workspace.to_string()))
}

/// Environment variable telling the media MCP which conversation it serves.
pub const MEDIA_CONVERSATION_ENV: &str = "AIONUI_MEDIA_CONVERSATION_ID";

/// The conversation variable to add for `server_name`, if it wants one.
///
/// Exists so a company can trace a media charge back to where it happened. The
/// job engine records the conversation on every usage report, but an
/// agent-initiated generation arrives over MCP — a stdio subprocess that
/// otherwise has no idea which conversation it belongs to — so without this the
/// ledger row for the most common path points at nothing. (Generations started
/// from the compose box carry it already: the renderer knows its own
/// conversation.)
///
/// Same narrow scoping as the workspace variable: only the bundled media server
/// gets it, never an arbitrary user-configured one.
pub fn media_conversation_env(server_name: &str, conversation_id: &str) -> Option<(String, String)> {
    if server_name != BUILTIN_MEDIA_MCP_NAME {
        return None;
    }
    let conversation_id = conversation_id.trim();
    if conversation_id.is_empty() {
        return None;
    }
    Some((MEDIA_CONVERSATION_ENV.to_string(), conversation_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without this the usage ledger row for an agent-initiated generation —
    /// the common case — points at no conversation, so a company can see that
    /// money was spent and nothing about where.
    #[test]
    fn media_server_is_told_which_conversation_it_serves() {
        let entry = media_conversation_env(BUILTIN_MEDIA_MCP_NAME, "conv-42").expect("media server");
        assert_eq!(entry.0, MEDIA_CONVERSATION_ENV);
        assert_eq!(entry.1, "conv-42");
    }

    /// Same narrow scoping as the workspace variable: telling arbitrary
    /// user-configured servers which conversation is running is nobody's
    /// business but the bundled media tool's.
    #[test]
    fn other_servers_are_not_told_the_conversation() {
        assert!(media_conversation_env("some-user-server", "conv-42").is_none());
        assert!(media_conversation_env("one-export-pdf", "conv-42").is_none());
    }

    #[test]
    fn an_absent_conversation_is_simply_omitted() {
        assert!(media_conversation_env(BUILTIN_MEDIA_MCP_NAME, "").is_none());
        assert!(media_conversation_env(BUILTIN_MEDIA_MCP_NAME, "   ").is_none());
    }

    #[test]
    fn media_server_gets_the_session_workspace() {
        let entry = media_workspace_env(BUILTIN_MEDIA_MCP_NAME, "D:/work/conv-1").expect("media server");

        assert_eq!(entry.0, MEDIA_WORKSPACE_ENV);
        assert_eq!(entry.1, "D:/work/conv-1");
    }

    /// Handing the workspace to arbitrary user-configured servers would leak
    /// where the user's files live; only the bundled media tool needs it.
    #[test]
    fn other_servers_are_not_told_where_the_user_works() {
        for name in ["chrome-devtools", "one-export-pdf", "one-team-knowledge", "ftshare"] {
            assert!(media_workspace_env(name, "D:/work/conv-1").is_none(), "{name}");
        }
    }

    #[test]
    fn an_absent_workspace_is_left_to_the_tools_own_fallback() {
        for workspace in ["", "   "] {
            assert!(media_workspace_env(BUILTIN_MEDIA_MCP_NAME, workspace).is_none());
        }
    }
}
