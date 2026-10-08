//! Integration tests against a fixture `/api/v1` server (axum, temp port):
//! 200/ETag/304/401/404 and incremental-cursor pagination, plus the
//! end-to-end plan/export/local-edit-refusal flow through `run`.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ai_memory_wikisync::state::{self, SyncState};
use ai_memory_wikisync::sync::{Mode, RunArgs, run};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;

/// Fixture server state: pages, optional bearer token, call counters.
struct Fixture {
    /// (path, body) in listing order.
    pages: Vec<(String, String)>,
    token: Option<&'static str>,
    project_exists: bool,
    /// Page advertised by the listing but missing on read, to simulate a
    /// delete/expiry racing the run.
    unreachable: Option<&'static str>,
    recent_calls: AtomicUsize,
    page_reads_200: AtomicUsize,
    page_reads_304: AtomicUsize,
}

impl Fixture {
    fn new(pages: Vec<(String, String)>) -> Self {
        Self {
            pages,
            token: None,
            project_exists: true,
            unreachable: None,
            recent_calls: AtomicUsize::new(0),
            page_reads_200: AtomicUsize::new(0),
            page_reads_304: AtomicUsize::new(0),
        }
    }

    fn with_token(mut self, token: &'static str) -> Self {
        self.token = Some(token);
        self
    }

    fn missing_project(mut self) -> Self {
        self.project_exists = false;
        self
    }

    fn with_unreachable(mut self, path: &'static str) -> Self {
        self.unreachable = Some(path);
        self
    }

    fn body_of(&self, path: &str) -> Option<&str> {
        self.pages
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, body)| body.as_str())
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), Box<axum::response::Response>> {
        let expected = match self.token {
            None => return Ok(()),
            Some(token) => format!("Bearer {token}"),
        };
        match headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        {
            Some(value) if value == expected => Ok(()),
            _ => Err(Box::new(error_json(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ))),
        }
    }
}

fn error_json(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Incremental `recent` listing with a page size of 2, so tests exercise
/// cursor pagination. The cursor is an opaque index string.
async fn recent_handler(
    State(fixture): State<Arc<Fixture>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> axum::response::Response {
    fixture.recent_calls.fetch_add(1, Ordering::SeqCst);
    if let Err(response) = fixture.authorize(&headers) {
        return *response;
    }
    if !fixture.project_exists || workspace != "demo" || project != "app" {
        return error_json(StatusCode::NOT_FOUND, "workspace or project not found");
    }
    // Ignore updated_since; the fixture serves everything after the epoch.
    let start: usize = query
        .get("cursor")
        .and_then(|value| value.strip_prefix("idx:"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let end = (start + 2).min(fixture.pages.len());
    let pages: Vec<serde_json::Value> = fixture.pages[start..end]
        .iter()
        .enumerate()
        .map(|(offset, (path, _))| {
            json!({
                "path": path,
                "title": format!("title {path}"),
                "kind": "note",
                "tier": "semantic",
                "updated_at": format!("2026-10-0{}T00:00:00Z", start + offset + 1),
            })
        })
        .collect();
    let next_cursor = if end < fixture.pages.len() {
        json!(format!("idx:{end}"))
    } else {
        json!(null)
    };
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(json!({ "pages": pages, "next_cursor": next_cursor })),
    )
        .into_response()
}

/// Page read with the server's exact-ETag 304 contract.
async fn page_handler(
    State(fixture): State<Arc<Fixture>>,
    Path((workspace, project, path)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(response) = fixture.authorize(&headers) {
        return *response;
    }
    if !fixture.project_exists || workspace != "demo" || project != "app" {
        return error_json(StatusCode::NOT_FOUND, "workspace or project not found");
    }
    if fixture.unreachable == Some(path.as_str()) {
        return error_json(StatusCode::NOT_FOUND, "page not found");
    }
    let Some(body) = fixture.body_of(&path) else {
        return error_json(StatusCode::NOT_FOUND, "page not found");
    };
    let etag = format!("\"{}\"", state::sha256_hex(body.as_bytes()));
    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && if_none_match == etag
    {
        fixture.page_reads_304.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag)],
            axum::body::Body::empty(),
        )
            .into_response();
    }
    fixture.page_reads_200.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (header::CONTENT_TYPE, "application/json".to_string()),
        ],
        Json(json!({
            "project": project,
            "workspace": workspace,
            "path": path,
            "title": format!("title {path}"),
            "kind": "note",
            "tier": "semantic",
            "pinned": false,
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": "2026-10-01T00:00:00Z",
            "supersedes": null,
            "frontmatter": {},
            "body_markdown": body,
            "links": [],
            "backlinks": [],
        })),
    )
        .into_response()
}

async fn serve(fixture: Fixture) -> (SocketAddr, Arc<Fixture>) {
    let shared = Arc::new(fixture);
    let app = Router::new()
        .route(
            "/api/v1/workspaces/{workspace}/projects/{project}/recent",
            get(recent_handler),
        )
        .route(
            "/api/v1/workspaces/{workspace}/projects/{project}/pages/{*path}",
            get(page_handler),
        )
        .with_state(shared.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let addr = listener.local_addr().expect("fixture addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fixture server");
    });
    (addr, shared)
}

fn args(addr: SocketAddr, dest: &std::path::Path, families: &[&str]) -> RunArgs {
    RunArgs {
        server: format!("http://{addr}"),
        token: None,
        workspace: "demo".to_string(),
        project: "app".to_string(),
        dest: dest.to_path_buf(),
        include: families.iter().map(|f| f.to_string()).collect(),
        force: false,
    }
}

fn fixture_pages() -> Vec<(String, String)> {
    vec![
        (
            "_rules/postgres.md".to_string(),
            "# Postgres only\nUse Postgres.\n".to_string(),
        ),
        (
            "decisions/0001-db.md".to_string(),
            "# Standardised on Postgres\nBody.\n".to_string(),
        ),
        ("notes/a.md".to_string(), "# A\nalpha\n".to_string()),
        ("notes/b.md".to_string(), "# B\nbeta\n".to_string()),
        ("notes/c.md".to_string(), "# C\ngamma\n".to_string()),
    ]
}

fn temp_dest() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalize so the symlink-free destination guard passes on every OS.
    let root = tmp.path().canonicalize().unwrap().join("wiki");
    (tmp, root)
}

#[tokio::test]
async fn plan_lists_actions_without_writing() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Plan)
        .await
        .expect("plan succeeds");
    assert!(!dest.exists(), "plan must not create the destination");
}

#[tokio::test]
async fn export_apply_writes_files_and_state() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();

    run(&args(addr, &dest, &["notes", "_rules"]), Mode::Apply)
        .await
        .expect("first export");

    assert_eq!(fs::read(dest.join("notes/a.md")).unwrap(), b"# A\nalpha\n");
    assert_eq!(
        fs::read(dest.join("_rules/postgres.md")).unwrap(),
        b"# Postgres only\nUse Postgres.\n"
    );
    // Only allowlisted families land on disk.
    assert!(!dest.join("decisions/0001-db.md").exists());

    let state = state::load(&dest).unwrap();
    assert_eq!(state.pages.len(), 4, "notes/* plus _rules/postgres.md");
    for path in [
        "notes/a.md",
        "notes/b.md",
        "notes/c.md",
        "_rules/postgres.md",
    ] {
        let entry = state.pages.get(path).expect(path);
        assert!(entry.etag.is_some(), "etag recorded for {path}");
    }
}

#[tokio::test]
async fn reexport_revalidates_with_etag_and_writes_nothing() {
    let (addr, fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("first export");
    let before = fs::read(dest.join("notes/a.md")).unwrap();
    let reads_before = fixture.page_reads_200.load(Ordering::SeqCst);

    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("second export");
    assert!(
        fixture.page_reads_304.load(Ordering::SeqCst) >= 3,
        "every unchanged page revalidated via 304"
    );
    assert_eq!(
        fixture.page_reads_200.load(Ordering::SeqCst),
        reads_before,
        "no full page body was fetched again"
    );
    assert_eq!(fs::read(dest.join("notes/a.md")).unwrap(), before);
}

#[tokio::test]
async fn local_edits_are_refused_until_forced() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("first export");

    fs::write(dest.join("notes/b.md"), b"# B\nlocally edited\n").unwrap();
    // A brand-new local file inside an allowlisted family is unknown, not
    // ours to clobber.
    fs::write(dest.join("notes/zz-new.md"), b"# local\n").unwrap();

    let refused = run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect_err("must refuse when files diverged");
    let message = refused.to_string();
    assert!(message.contains("diverged"), "{message}");
    assert_eq!(
        fs::read(dest.join("notes/b.md")).unwrap(),
        b"# B\nlocally edited\n",
        "diverged file is untouched"
    );
    assert_eq!(fs::read(dest.join("notes/a.md")).unwrap(), b"# A\nalpha\n");
    assert_eq!(
        fs::read(dest.join("notes/zz-new.md")).unwrap(),
        b"# local\n",
        "unknown local file is untouched"
    );

    let mut force_args = args(addr, &dest, &["notes"]);
    force_args.force = true;
    run(&force_args, Mode::Apply)
        .await
        .expect("--force exports");
    assert_eq!(fs::read(dest.join("notes/b.md")).unwrap(), b"# B\nbeta\n");
    assert_eq!(
        fs::read(dest.join("notes/zz-new.md")).unwrap(),
        b"# local\n",
        "--force overwrites divergent server pages; it still never deletes"
    );
    // State reflects the forced page.
    let state = state::load(&dest).unwrap();
    assert_eq!(
        state.pages["notes/b.md"].hash,
        state::sha256_hex(b"# B\nbeta\n")
    );
}

#[tokio::test]
async fn unauthorized_is_a_clear_error() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages()).with_token("sekrit")).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Plan)
        .await
        .expect_err("must fail");
    let message = err.to_string();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("AI_MEMORY_AUTH_TOKEN"), "{message}");
    assert!(!message.contains("sekrit"), "token never leaks: {message}");

    let mut authed = args(addr, &dest, &["notes"]);
    authed.token = Some("sekrit".to_string());
    run(&authed, Mode::Plan).await.expect("authorized plan");
}

#[tokio::test]
async fn unknown_scope_is_a_clear_404() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages()).missing_project()).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Plan)
        .await
        .expect_err("must fail");
    let message = err.to_string();
    assert!(message.contains("404"), "{message}");
    assert!(message.contains("--workspace/--project"), "{message}");
}

#[tokio::test]
async fn listing_follows_cursors_to_the_last_page() {
    // Five pages with a fixture page size of 2 means three listing rounds.
    let (addr, fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("export");
    assert!(
        fixture.recent_calls.load(Ordering::SeqCst) >= 3,
        "cursor pagination was exercised"
    );
    assert!(dest.join("notes/c.md").exists(), "last page reached");
}

#[tokio::test]
async fn server_page_paths_escaping_dest_are_refused() {
    let hostile = vec![
        ("notes/ok.md".to_string(), "# ok\n".to_string()),
        (
            "notes/../../escape.md".to_string(),
            "# escape\n".to_string(),
        ),
    ];
    let (addr, _fixture) = serve(Fixture::new(hostile)).await;
    let (tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect_err("hostile listing refused");
    assert!(err.to_string().contains("refused"), "{err}");
    // The batch failed during selection: no page files, nothing outside dest.
    assert!(!dest.join("notes/ok.md").exists());
    assert!(
        !tmp.path().join("escape.md").exists(),
        "nothing outside dest"
    );
}

#[tokio::test]
async fn case_fold_collisions_from_server_are_refused() {
    let colliding = vec![
        ("notes/a.md".to_string(), "# a\n".to_string()),
        ("Notes/A.md".to_string(), "# A\n".to_string()),
    ];
    let (addr, _fixture) = serve(Fixture::new(colliding)).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes", "Notes"]), Mode::Apply)
        .await
        .expect_err("collision refused");
    assert!(err.to_string().contains("case-fold"), "{}", err);
}

#[tokio::test]
async fn allowlist_rules_apply_end_to_end() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();

    let none = RunArgs {
        include: vec![],
        ..args(addr, &dest, &[])
    };
    let err = run(&none, Mode::Plan).await.expect_err("empty allowlist");
    assert!(err.to_string().contains("must be explicit"), "{err}");

    let star = RunArgs {
        include: vec!["*".to_string()],
        ..args(addr, &dest, &[])
    };
    let err = run(&star, Mode::Plan).await.expect_err("bare star refused");
    assert!(err.to_string().contains("'*'"), "{err}");
}

#[tokio::test]
async fn page_missing_on_read_fails_loudly() {
    // The listing advertises a page the read cannot find (deleted or
    // expired mid-run): the run must fail instead of skipping silently.
    let mut pages = fixture_pages();
    pages.push(("notes/ghost.md".to_string(), "# ghost\n".to_string()));
    let (addr, _fixture) = serve(Fixture::new(pages).with_unreachable("notes/ghost.md")).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect_err("ghost page must fail the run");
    assert!(
        err.to_string().contains("404") || err.to_string().contains("ghost"),
        "{err}"
    );
    assert!(
        !dest.join("notes/a.md").exists(),
        "a failed batch writes nothing"
    );
}

#[tokio::test]
async fn empty_state_after_user_deletes_local_file_recreates_it() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("first export");
    fs::remove_file(dest.join("notes/b.md")).unwrap();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("recreate");
    assert_eq!(fs::read(dest.join("notes/b.md")).unwrap(), b"# B\nbeta\n");
    let state: SyncState = state::load(&dest).unwrap();
    assert!(state.pages.contains_key("notes/b.md"));
}
