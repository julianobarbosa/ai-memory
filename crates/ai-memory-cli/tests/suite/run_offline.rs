//! End-to-end coverage for degraded offline launches: `ai-memory run` against
//! a closed port (a homelab down for maintenance) must WARN and still launch
//! the harness without the server, `--require-server` must fail closed with
//! the old augmented error, and offline auto-wire must install hooks but no
//! dead-server MCP registration. Unix-only: the fake harness is a shebang
//! script.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

/// A loopback address nothing listens on: `free_port` binds and releases.
fn closed_port_url() -> String {
    format!("http://127.0.0.1:{}", crate::e2e_support::free_port())
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn host_tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} on PATH"))
}

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    claude_ran: PathBuf,
    claude_env: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let home = root.join("home");
        let bin = root.join("bin");
        for dir in [&repo, &home, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init");
        std::os::unix::fs::symlink(host_tool("git"), bin.join("git")).unwrap();

        let claude_ran = root.join("claude-ran.txt");
        let claude_env = root.join("claude-env.txt");
        // The fake harness records its argv and environment and writes the
        // (empty-session) transcript the launcher waits for, so the run
        // finishes instead of waiting out the transcript-flush poll. `set`
        // (a shell builtin) dumps the environment without needing anything
        // else on the replaced PATH.
        let transcripts = home
            .join(".claude/projects")
            .join(repo.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&transcripts).unwrap();
        write_script(
            &bin.join("claude"),
            &format!(
                "printf '%s\\n' \"$@\" > '{ran}'\nset > '{env_}'\n\
                 while [ $# -gt 0 ]; do\n\
                 if [ \"$1\" = --session-id ]; then\n\
                 printf '{{\"sessionId\":\"%s\",\"cwd\":\"%s\"}}\\n' \"$2\" '{repo}' > '{dir}'/\"$2\".jsonl\n\
                 fi\nshift\ndone\nexit 0\n",
                ran = claude_ran.display(),
                env_ = claude_env.display(),
                repo = repo.display(),
                dir = transcripts.display(),
            ),
        );
        Self {
            _temp: temp,
            repo,
            home,
            bin,
            claude_ran,
            claude_env,
        }
    }
}

fn command(fixture: &Fixture, server: &str, args: &[&str]) -> tokio::process::Command {
    let mut command: tokio::process::Command = crate::e2e_support::hermetic(BIN).into();
    for name in ["SSH_AUTH_SOCK", "XDG_CONFIG_HOME", "GH_CONFIG_DIR", "PS1"] {
        command.env_remove(name);
    }
    command
        .args(args)
        .current_dir(&fixture.repo)
        .env("PATH", &fixture.bin)
        .env("HOME", &fixture.home)
        .env("AI_MEMORY_HOME", &fixture.home)
        .env("CLAUDE_CONFIG_DIR", fixture.home.join(".claude"))
        .env("AI_MEMORY_DATA_DIR", fixture.home.join("data"))
        .env("AI_MEMORY_SERVER_URL", server)
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

async fn run(fixture: &Fixture, server: &str, args: &[&str]) -> Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command(fixture, server, args).output(),
    )
    .await
    .expect("ai-memory run finished")
    .expect("spawn ai-memory")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A closed port downgrades the launch: one loud warning, the harness runs,
/// no run/workstream ids are attributed to the offline child, and the
/// not-recorded line accounts for the (empty) local spool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_run_launches_the_harness_without_the_server() {
    let fixture = Fixture::new();
    let server = closed_port_url();
    let output = run(&fixture, &server, &["run", "--no-autowire", "claude"]).await;
    assert!(output.status.success(), "{}", stderr(&output));

    let stderr = stderr(&output);
    assert!(
        stderr.contains("WARNING") && stderr.contains("unreachable"),
        "the degraded warning is loud:\n{stderr}"
    );
    assert!(
        stderr.contains(&server),
        "the warning names the server URL:\n{stderr}"
    );
    assert!(
        stderr.contains("not recorded on the server"),
        "the spool account is printed:\n{stderr}"
    );
    assert!(
        stderr.contains("no hook events remain spooled locally"),
        "the empty spool is reported as empty:\n{stderr}"
    );

    assert!(fixture.claude_ran.exists(), "the harness ran:\n{stderr}");
    let child_env = fs::read_to_string(&fixture.claude_env).unwrap();
    assert!(
        !child_env
            .lines()
            .any(|line| line.starts_with("AI_MEMORY_RUN_ID=")
                || line.starts_with("AI_MEMORY_WORKSTREAM_ID=")),
        "no server-run attribution may reach the offline child:\n{child_env}"
    );
    assert!(
        child_env
            .lines()
            .any(|line| line.starts_with("AI_MEMORY_HOOK_URL=")),
        "the hook URL still points at the configured server:\n{child_env}"
    );
}

/// `--require-server` restores the fail-closed behavior: the old augmented
/// connect error (including "the agent was not started"), and the harness
/// never starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_server_fails_closed_when_offline() {
    let fixture = Fixture::new();
    let server = closed_port_url();
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "--require-server", "claude"],
    )
    .await;
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(
        stderr.contains("the agent was not started"),
        "the prepare context is preserved:\n{stderr}"
    );
    assert!(
        stderr.contains("could not reach"),
        "the augmented connect diagnosis is reused:\n{stderr}"
    );
    assert!(
        !fixture.claude_ran.exists(),
        "the harness must not run:\n{stderr}"
    );
}

/// Offline auto-wire installs the hooks (capture must spool locally) but
/// registers no MCP entry for the unreachable server and leaves no sentinel,
/// so the next online launch completes the wiring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_autowire_wires_hooks_but_not_mcp() {
    let fixture = Fixture::new();
    let server = closed_port_url();
    let output = run(&fixture, &server, &["run", "claude"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("wiring its ai-memory hooks"),
        "the offline wiring is announced:\n{stderr}"
    );
    assert!(
        stderr.contains("MCP registration waits"),
        "the deferred MCP is announced:\n{stderr}"
    );

    let settings = fs::read_to_string(fixture.home.join(".claude/settings.json"))
        .expect("hooks settings written");
    assert!(
        settings.contains("ai-memory") || settings.contains("ai_memory"),
        "hooks are wired offline: {settings}"
    );
    // With CLAUDE_CONFIG_DIR set, the Claude Code MCP target is
    // `<config_dir>/.claude.json`; check both spellings so the assertion
    // cannot pass vacuously.
    for mcp_path in [
        fixture.home.join(".claude.json"),
        fixture.home.join(".claude/.claude.json"),
    ] {
        assert!(
            !mcp_path.exists() || !fs::read_to_string(&mcp_path).unwrap().contains("ai-memory"),
            "no dead-server MCP registration may be written at {}",
            mcp_path.display()
        );
    }
    let state = fixture.home.join("data/autowire-state");
    assert!(
        !state.exists() || fs::read_dir(&state).unwrap().count() == 0,
        "an offline half-wiring must not gate the next online launch"
    );
}

/// The mid-run outage repair path: when the server dies after prepare, the
/// child's exit code survives, the warning points at `finalize-session`, and
/// the recorded exit status stays the child's own. The mock server answers
/// prepare, then closes its listener once the run is linked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mid_run_outage_preserves_the_child_exit_code() {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let trigger = shutdown_tx.clone();
    let app = Router::new()
        .route(
            "/workstream/runs",
            post(|| async {
                Json(json!({
                    "workstream_id": "12345678-1234-4234-9234-123456789abd",
                    "workstream_name": "fixture",
                    "run_id": "12345678-1234-4234-9234-123456789abe",
                    "resolved_agent": "claude-code",
                    "sync_after": 0, "sync_through": 0,
                    "may_adopt_existing_session": false,
                }))
            }),
        )
        .route(
            "/workstream/runs/{run_id}/link",
            post(move || {
                let _ = trigger.send(true);
                async { StatusCode::NO_CONTENT }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_rx;
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        break;
                    }
                }
            })
            .await
            .unwrap();
    });

    let fixture = Fixture::new();
    // Hold the child long enough for the graceful shutdown to close the
    // listener before the finish POSTs begin, then exit 5.
    write_script(
        &fixture.bin.join("claude"),
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\nsleep 1\nexit 5\n",
            fixture.claude_ran.display(),
        ),
    );
    let server_url = format!("http://{address}");
    let output = run(&fixture, &server_url, &["run", "--no-autowire", "claude"]).await;
    assert_eq!(
        output.status.code(),
        Some(5),
        "the child's exit code is preserved, not an ai-memory error:\n{}",
        stderr(&output)
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("was not imported"),
        "the outage is reported:\n{stderr}"
    );
    assert!(
        stderr.contains("ai-memory finalize-session"),
        "the repair path is named:\n{stderr}"
    );
    server.abort();
}
