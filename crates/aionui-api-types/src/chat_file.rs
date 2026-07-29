//! Chat-message file attachments.
//!
//! Two shapes are accepted because two generations of client exist:
//!
//! * a bare absolute path — every client before the project-scoped Explorer;
//! * a tagged object — what the current desktop client sends for every
//!   attachment (`common/types/chatFile.ts`), discriminated by `kind`:
//!   - explorer tree selections → [`TaggedChatFileRef::Project`] (resolved
//!     server-side via `resolve_reference(op = Read)`),
//!   - upload-button files → [`TaggedChatFileRef::Upload`] (always `upload`,
//!     carrying the absolute path returned by `POST /api/fs/upload`),
//!   - host-filesystem picker selections → [`TaggedChatFileRef::Local`] (an
//!     absolute path the user explicitly chose in the backend-machine file
//!     browser).
//!
//! The tagged form is not cosmetic: a `project` entry identifies a file by
//! `(pe_id, relative_path)` rather than by an absolute path, so it survives the
//! project root moving and is checked for containment when resolved. Accepting
//! only the bare form is what made *every* attachment fail with
//! `400 Invalid JSON request body` — the client had already moved to the tagged
//! shape while this struct still required `Vec<String>`.

use serde::{Deserialize, Serialize};

/// A single file attached to a chat message.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ChatFileRef {
    /// Absolute path on the backend host, sent as-is.
    Path(String),
    Tagged(TaggedChatFileRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaggedChatFileRef {
    /// A file inside a bound project folder, addressed by explorer identity
    /// (`pe_id` + `relative_path`). The backend resolves it to an absolute path
    /// via `resolve_reference` with lexical + realpath containment.
    Project { pe_id: String, relative_path: String },
    /// An uploaded file, carried as the absolute path returned by
    /// `POST /api/fs/upload`. The backend requires it to live under the managed
    /// upload directory before use.
    Upload { path: String },
    /// A file on the backend machine's filesystem, chosen by the user in the
    /// host-file browser (`/api/fs/browse`, which already exposes the whole
    /// filesystem). Carries an absolute path; the backend only checks it exists
    /// and is a regular file — no managed-directory restriction, since the
    /// picker that produced it already exposes this surface and the agent reads
    /// the path through its own filesystem tools.
    Local { path: String },
}

impl ChatFileRef {
    /// The path this ref carries directly, if it has one. `project` refs return
    /// `None` — they need the project store to become a path, which happens in
    /// the service layer where that dependency is available.
    pub fn direct_path(&self) -> Option<&str> {
        match self {
            Self::Path(path) => Some(path),
            Self::Tagged(TaggedChatFileRef::Upload { path } | TaggedChatFileRef::Local { path }) => Some(path),
            Self::Tagged(TaggedChatFileRef::Project { .. }) => None,
        }
    }

    /// The `(pe_id, relative_path)` pair for a project ref.
    pub fn project_ref(&self) -> Option<(&str, &str)> {
        match self {
            Self::Tagged(TaggedChatFileRef::Project { pe_id, relative_path }) => {
                Some((pe_id.as_str(), relative_path.as_str()))
            }
            _ => None,
        }
    }
}
