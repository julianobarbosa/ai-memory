//! `ai-memory-wikisync`: read-only team-wiki export companion (#986, slice 1).
//!
//! Pulls explicitly allowlisted page families from a running ai-memory server
//! through the public, read-only `/api/v1` surface into a directory inside a
//! project repository. The tool never mutates the server, never deletes local
//! files (deletes are slice 4), never forges page frontmatter, and never runs
//! git on the operator's behalf.
//!
//! Page bodies are untrusted data: they are transported verbatim into files
//! whose validated paths cannot escape the destination directory, and are
//! never executed or rendered.

pub mod client;
pub mod paths;
pub mod state;
pub mod sync;

/// Hard ceiling on pages handled in one run. A hostile or misconfigured
/// server cannot make the export loop or write unboundedly.
pub const MAX_PAGES: usize = 10_000;
/// Hard ceiling on one page body. The server's own HTTP cap is 10 MiB, so
/// anything near this bound already indicates a broken projection.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Directory (inside `--dest`) holding the single local state file.
pub const STATE_DIR: &str = ".ai-memory-wikisync";
