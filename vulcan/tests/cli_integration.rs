//! End-to-end CLI integration tests against a hostile environment.
//!
//! These spawn the real `vulcan` binary (resolved via `CARGO_BIN_EXE_vulcan`)
//! with deliberately stripped env vars — empty `VULCAN_WALLET_PASSWORD`, a
//! nonexistent `HOME`, no `XDG_CONFIG_HOME` — and assert that the user sees a
//! friendly, actionable error envelope. These bugs class are the ones that
//! bite agents in production (plugin host passes blank `userConfig`, CI
//! shell has no `HOME`, etc.) and unit tests cannot reach them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vulcan"))
}

fn stripped_env(cmd: &mut Command, fake_home: &std::path::Path) {
    cmd.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", fake_home)
        // Make sure XDG dirs don't leak from the test host
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("VULCAN_WALLET_NAME")
        .env_remove("VULCAN_WALLET_PASSWORD");
}

const TEST_WALLET_PASSWORD: &str = "vulcan-test-password";

/// Run any command with the test wallet password and parse the JSON envelope.
fn run_json(fake_home: &Path, args: &[&str]) -> serde_json::Value {
    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, fake_home);
    cmd.env("VULCAN_WALLET_PASSWORD", TEST_WALLET_PASSWORD)
        .args(["--yes", "-o", "json"])
        .args(args);
    let out = cmd.output().expect("spawn vulcan");
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn create_default_local_wallet(fake_home: &Path) {
    let mut create = Command::new(bin());
    stripped_env(&mut create, fake_home);
    let out = create
        .env("VULCAN_WALLET_PASSWORD", "vulcan-test-password")
        .args([
            "--yes", "-o", "json", "wallet", "create", "--name", "mcp-test",
        ])
        .output()
        .expect("spawn vulcan wallet create");
    assert!(
        out.status.success(),
        "wallet create failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let mut set_default = Command::new(bin());
    stripped_env(&mut set_default, fake_home);
    let out = set_default
        .args(["--yes", "-o", "json", "wallet", "set-default", "mcp-test"])
        .output()
        .expect("spawn vulcan wallet set-default");
    assert!(
        out.status.success(),
        "wallet set-default failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn version_succeeds_with_minimal_env() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    let out = cmd.arg("version").output().expect("spawn vulcan");
    assert!(
        out.status.success(),
        "vulcan version failed under minimal env: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn live_mcp_without_password_fails_fast_with_actionable_error() {
    // Plugin hosts (Claude Code with blank userConfig fields) pass an empty
    // string for VULCAN_WALLET_PASSWORD. With `--allow-dangerous`, the MCP
    // server used to call wallet decrypt with "" and surface DECRYPT_FAILED.
    // The fix in main.rs treats empty as unset and fails with a clear
    // WALLET_PASSWORD_REQUIRED that explains both remedies (host UI + env var).
    let tmp = tempfile::tempdir().unwrap();
    create_default_local_wallet(tmp.path());

    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    cmd.env("VULCAN_WALLET_PASSWORD", "") // explicitly empty
        .args(["mcp", "--allow-dangerous"]);
    let out = cmd.output().expect("spawn vulcan mcp");

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "mcp should fail without password");
    assert!(
        combined.contains("WALLET_PASSWORD_REQUIRED"),
        "expected WALLET_PASSWORD_REQUIRED error code, got:\n{combined}"
    );
    // The error message must guide the user to BOTH remedies (plugin UI + env var).
    assert!(
        combined.contains("plugin") || combined.contains("Wallet password"),
        "error should mention the plugin remedy, got:\n{combined}"
    );
    assert!(
        combined.contains("VULCAN_WALLET_PASSWORD"),
        "error should mention the env var remedy, got:\n{combined}"
    );
}

#[test]
fn paper_safe_mcp_starts_and_lists_only_non_dangerous_tools() {
    // The plugin-default path: `vulcan mcp` with no flags and no wallet env.
    // Server must start and serve tools/list, with dangerous tools hidden.
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    cmd.arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn vulcan mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let handshake = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    ];
    for line in &handshake {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    }
    stdin.flush().unwrap();
    drop(stdin);

    // Read until EOF, but bound by wall-clock.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = child.wait_with_output().expect("wait_with_output");
        tx.send(out).ok();
    });
    let out = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("MCP server did not exit within 15s");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // Find the tools/list reply (id == 2)
    let tool_names: Vec<String> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter(|v| v.get("id").and_then(|i| i.as_i64()) == Some(2))
        .flat_map(|v| {
            v.get("result")
                .and_then(|r| r.get("tools"))
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect();

    assert!(
        !tool_names.is_empty(),
        "tools/list returned nothing, stdout was:\n{stdout}"
    );
    // Locked-in invariants on visibility:
    let must_be_visible = [
        "vulcan_market_ticker",
        "vulcan_market_candles",
        "vulcan_ta_compute",
        "vulcan_strategy_preflight",
        "vulcan_paper_status",
    ];
    for name in must_be_visible {
        assert!(
            tool_names.iter().any(|n| n == name),
            "read-only tool `{name}` should be visible without --allow-dangerous; got {tool_names:?}"
        );
    }
    let must_be_hidden = [
        "vulcan_trade",
        "vulcan_margin_deposit",
        "vulcan_margin_withdraw",
        "vulcan_position_close",
    ];
    for name in must_be_hidden {
        assert!(
            !tool_names.iter().any(|n| n == name),
            "dangerous tool `{name}` should be HIDDEN without --allow-dangerous; got {tool_names:?}"
        );
    }
}

#[test]
fn mcp_diagnose_reports_failure_when_no_config_exists() {
    // Diagnose should never panic on a missing config file; it should report
    // a clean NOT-CONFIGURED verdict with the install command as remedy.
    let tmp = tempfile::tempdir().unwrap();
    let bogus_path = tmp.path().join("nonexistent.json");
    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    cmd.args([
        "agent", "mcp", "diagnose", "--target", "claude", "--scope", "user", "--path",
    ])
    .arg(&bogus_path)
    .args(["-o", "json"]);
    let out = cmd.output().expect("spawn vulcan agent mcp diagnose");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("not JSON: {stdout}"));
    assert_eq!(v["ok"], true, "envelope: {v}");
    assert_eq!(v["data"]["configured"], false);
    assert_eq!(v["data"]["passed"], false);
    let remedies = v["data"]["remedies"].as_array().expect("remedies array");
    assert!(
        remedies.iter().any(|r| r
            .as_str()
            .unwrap_or_default()
            .contains("vulcan agent mcp install")),
        "remedies should suggest `vulcan agent mcp install`, got {remedies:?}"
    );
}

#[test]
#[cfg(unix)]
fn agent_mcp_install_writes_file_with_0600_perms() {
    use std::os::unix::fs::PermissionsExt;

    // Non-dangerous install doesn't need a TTY/password — perfect for testing
    // the atomic_write permissions hardening end-to-end.
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("plugin-config.json");
    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    cmd.args([
        "agent", "mcp", "install", "--target", "cursor", "--scope", "user", "--path",
    ])
    .arg(&config_path)
    .args(["-o", "json"]);
    let out = cmd.output().expect("spawn vulcan agent mcp install");
    assert!(
        out.status.success(),
        "install failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let meta = std::fs::metadata(&config_path).expect("config file should exist");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o600,
        "MCP config should be mode 0600, got 0{mode:o} at {}",
        config_path.display()
    );
}

// ── Surfnet (local Surfpool fork) ────────────────────────────────────────

/// Point Vulcan's recorded Surfnet at a closed port so every check is hermetic.
fn write_dead_surfnet_state(fake_home: &Path) {
    let dir = fake_home.join(".vulcan");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("surfnet.json"),
        r#"{"rpc_url":"http://127.0.0.1:1","ws_url":"ws://127.0.0.1:2","network":"mainnet","pid":null,"started_at":"2026-01-01T00:00:00Z","log_path":"/dev/null"}"#,
    )
    .unwrap();
}

#[test]
fn surfnet_status_reports_not_running_without_a_fork() {
    let tmp = tempfile::tempdir().unwrap();
    write_dead_surfnet_state(tmp.path());

    let v = run_json(tmp.path(), &["surfnet", "status"]);
    assert_eq!(v["ok"], true, "envelope: {v}");
    assert_eq!(v["data"]["running"], false, "envelope: {v}");
    assert_eq!(v["data"]["rpc_url"], "http://127.0.0.1:1");
}

#[test]
fn surfnet_fund_fails_clearly_when_no_fork_is_running() {
    let tmp = tempfile::tempdir().unwrap();
    create_default_local_wallet(tmp.path());
    write_dead_surfnet_state(tmp.path());

    let v = run_json(tmp.path(), &["surfnet", "fund", "mcp-test", "--sol", "1"]);
    assert_eq!(v["ok"], false, "envelope: {v}");
    assert_eq!(v["error"]["code"], "SURFNET_NOT_RUNNING", "envelope: {v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("vulcan surfnet start"),
        "should point at the remedy: {v}"
    );
}

#[test]
fn surfnet_scenario_example_round_trips_through_validate() {
    let tmp = tempfile::tempdir().unwrap();

    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    let out = cmd
        .args(["surfnet", "scenario", "example"])
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let file = tmp.path().join("scenario.toml");
    std::fs::write(&file, &out.stdout).unwrap();

    let v = run_json(
        tmp.path(),
        &["surfnet", "scenario", "validate", file.to_str().unwrap()],
    );
    assert_eq!(v["ok"], true, "envelope: {v}");
    assert_eq!(v["data"]["name"], "funded-trader");
    let steps = v["data"]["steps"].as_array().expect("steps");
    assert!(steps
        .iter()
        .any(|s| s.as_str().unwrap_or_default().starts_with("fund trader")));
    assert!(steps
        .iter()
        .any(|s| s.as_str().unwrap_or_default() == "pause the clock"));
}

#[test]
fn surfnet_scenario_validate_rejects_bad_files() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("bad.toml");
    std::fs::write(&file, "name = \"x\"\n[[fund]]\nsol = 1\n").unwrap();

    let v = run_json(
        tmp.path(),
        &["surfnet", "scenario", "validate", file.to_str().unwrap()],
    );
    assert_eq!(v["ok"], false, "envelope: {v}");
    assert_eq!(v["error"]["code"], "SCENARIO_INVALID", "envelope: {v}");
}

#[test]
fn checked_in_example_scenario_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scenarios/funded-trader.toml");

    let v = run_json(
        tmp.path(),
        &["surfnet", "scenario", "validate", file.to_str().unwrap()],
    );
    assert_eq!(v["ok"], true, "envelope: {v}");
}

#[test]
fn surfnet_flag_conflicts_with_rpc_url() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(bin());
    stripped_env(&mut cmd, tmp.path());
    let out = cmd
        .args(["--surfnet", "--rpc-url", "http://127.0.0.1:1", "version"])
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "clap must reject --surfnet with --rpc-url"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot be used with"), "stderr: {stderr}");
}

#[test]
fn surfnet_flag_routes_rpc_to_recorded_fork() {
    let tmp = tempfile::tempdir().unwrap();
    create_default_local_wallet(tmp.path());
    write_dead_surfnet_state(tmp.path());

    // With --surfnet the balance read goes to the recorded (dead) Surfnet URL,
    // so the failure must be an RPC error against 127.0.0.1:1, not mainnet.
    let v = run_json(tmp.path(), &["--surfnet", "wallet", "balance"]);
    assert_eq!(v["ok"], false, "envelope: {v}");
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("127.0.0.1:1"),
        "should target the dead Surfnet: {v}"
    );
}
