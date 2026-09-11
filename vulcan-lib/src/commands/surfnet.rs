//! Surfnet: a local Surfpool fork of mainnet for scenario testing.
//!
//! Surfpool (https://surfpool.run) runs a local Solana network that fetches
//! mainnet accounts on demand and exposes `surfnet_*` cheatcode RPCs for
//! patching balances, jumping the clock, and exporting snapshots. Vulcan wraps
//! the pieces an agent needs to build reproducible trading scenarios:
//! start/stop the fork, fund wallets, travel in time, snapshot state, and apply
//! declarative scenario files. Every other Vulcan command can target the fork
//! with the global `--surfnet` flag (or `--rpc-url`).

use crate::cli::surfnet::{
    ClockAction, ForkNetwork, ScenarioCommand, StartArgs, SurfnetCommand, TimeTravelArgs,
};
use crate::context::AppContext;
use crate::error::VulcanError;
use crate::output::{render_success, TableRenderable};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

pub const DEFAULT_SURFNET_RPC_URL: &str = "http://127.0.0.1:8899";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const CLOCK_SYSVAR: &str = "SysvarC1ock11111111111111111111111111111111";

// ── Persistent state (~/.vulcan/surfnet.json) ───────────────────────────

/// Record of the Surfnet started by `vulcan surfnet start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfnetState {
    pub rpc_url: String,
    pub ws_url: String,
    pub network: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datasource_rpc_url: Option<String>,
    pub pid: Option<u32>,
    pub started_at: String,
    pub log_path: PathBuf,
    #[serde(default)]
    pub skip_signature_verification: bool,
}

pub fn state_path(vulcan_dir: &Path) -> PathBuf {
    vulcan_dir.join("surfnet.json")
}

pub fn load_state(vulcan_dir: &Path) -> Result<Option<SurfnetState>, VulcanError> {
    let path = state_path(vulcan_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| VulcanError::io("SURFNET_STATE_READ_FAILED", e.to_string()))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| VulcanError::io("SURFNET_STATE_INVALID", e.to_string()))
}

fn save_state(vulcan_dir: &Path, state: &SurfnetState) -> Result<(), VulcanError> {
    let raw = serde_json::to_string_pretty(state)
        .map_err(|e| VulcanError::internal("SURFNET_STATE_ENCODE_FAILED", e.to_string()))?;
    std::fs::write(state_path(vulcan_dir), raw)
        .map_err(|e| VulcanError::io("SURFNET_STATE_WRITE_FAILED", e.to_string()))
}

fn clear_state(vulcan_dir: &Path) -> Result<(), VulcanError> {
    let path = state_path(vulcan_dir);
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|e| VulcanError::io("SURFNET_STATE_WRITE_FAILED", e.to_string()))?;
    }
    Ok(())
}

/// RPC URL other commands should use when `--surfnet` is passed: the running
/// Surfnet's URL if one was started through Vulcan, else the Surfpool default.
pub fn surfnet_rpc_url(vulcan_dir: &Path) -> String {
    load_state(vulcan_dir)
        .ok()
        .flatten()
        .map(|s| s.rpc_url)
        .unwrap_or_else(|| DEFAULT_SURFNET_RPC_URL.to_string())
}

// ── JSON-RPC client for surfnet_* cheatcodes ────────────────────────────

pub struct SurfnetRpc {
    url: String,
    client: reqwest::Client,
}

impl SurfnetRpc {
    pub fn new(url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            url: url.into(),
            client,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, VulcanError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                VulcanError::network(
                    "SURFNET_UNREACHABLE",
                    format!("Surfnet at {} did not answer `{method}`: {e}. Start one with `vulcan surfnet start`.", self.url),
                )
            })?;
        let payload: Value = response
            .json()
            .await
            .map_err(|e| VulcanError::api("SURFNET_RPC_INVALID_RESPONSE", e.to_string()))?;
        if let Some(err) = payload.get("error") {
            return Err(VulcanError::api(
                "SURFNET_RPC_ERROR",
                format!(
                    "`{method}` failed: {}",
                    err["message"].as_str().unwrap_or("unknown error")
                ),
            ));
        }
        Ok(payload.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn is_healthy(&self) -> bool {
        matches!(self.call("getHealth", json!([])).await, Ok(Value::String(s)) if s == "ok")
    }

    pub async fn slot(&self) -> Result<u64, VulcanError> {
        self.call("getSlot", json!([]))
            .await?
            .as_u64()
            .ok_or_else(|| VulcanError::api("SURFNET_RPC_ERROR", "getSlot returned no slot"))
    }

    /// Current on-chain clock (slot, unix timestamp) from the Clock sysvar.
    pub async fn clock(&self) -> Result<(u64, i64), VulcanError> {
        let result = self
            .call(
                "getAccountInfo",
                json!([CLOCK_SYSVAR, { "encoding": "base64" }]),
            )
            .await?;
        let data_b64 = result["value"]["data"][0]
            .as_str()
            .ok_or_else(|| VulcanError::api("SURFNET_RPC_ERROR", "clock sysvar missing"))?;
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| VulcanError::api("SURFNET_RPC_ERROR", e.to_string()))?;
        clock_from_sysvar_bytes(&bytes)
    }

    pub async fn set_lamports(&self, pubkey: &Pubkey, lamports: u64) -> Result<(), VulcanError> {
        self.call(
            "surfnet_setAccount",
            set_account_params(pubkey, json!({ "lamports": lamports })),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_account(&self, pubkey: &Pubkey, update: Value) -> Result<(), VulcanError> {
        self.call("surfnet_setAccount", set_account_params(pubkey, update))
            .await
            .map(|_| ())
    }

    pub async fn set_token_amount(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
        amount: u64,
    ) -> Result<(), VulcanError> {
        self.call(
            "surfnet_setTokenAccount",
            set_token_account_params(owner, mint, amount),
        )
        .await
        .map(|_| ())
    }

    pub async fn time_travel(&self, target: TimeTravelTarget) -> Result<Value, VulcanError> {
        self.call("surfnet_timeTravel", json!([target.to_param()]))
            .await
    }

    pub async fn pause_clock(&self) -> Result<Value, VulcanError> {
        self.call("surfnet_pauseClock", json!([])).await
    }

    pub async fn resume_clock(&self) -> Result<Value, VulcanError> {
        self.call("surfnet_resumeClock", json!([])).await
    }

    /// Export the account map (`pubkey -> account`) in the format `surfpool
    /// start --snapshot` accepts. The RPC wraps it in `{ context, value }`.
    pub async fn export_snapshot(&self) -> Result<Value, VulcanError> {
        let result = self
            .call("surfnet_exportSnapshot", json!([{ "scope": "network" }]))
            .await?;
        Ok(unwrap_rpc_value(result))
    }

    pub async fn reset_network(&self) -> Result<(), VulcanError> {
        self.call("surfnet_resetNetwork", json!([]))
            .await
            .map(|_| ())
    }

    pub async fn info(&self) -> Result<Value, VulcanError> {
        self.call("surfnet_getSurfnetInfo", json!([])).await
    }
}

// ── Pure helpers (unit-tested) ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeTravelTarget {
    Slot(u64),
    /// Unix seconds; Surfpool expects milliseconds on the wire.
    Timestamp(i64),
}

impl TimeTravelTarget {
    pub fn to_param(self) -> Value {
        match self {
            TimeTravelTarget::Slot(slot) => json!({ "absoluteSlot": slot }),
            TimeTravelTarget::Timestamp(secs) => json!({ "absoluteTimestamp": secs * 1000 }),
        }
    }
}

pub fn set_account_params(pubkey: &Pubkey, update: Value) -> Value {
    json!([pubkey.to_string(), update])
}

pub fn set_token_account_params(owner: &Pubkey, mint: &Pubkey, amount: u64) -> Value {
    json!([owner.to_string(), mint.to_string(), { "amount": amount }])
}

pub fn sol_to_lamports(sol: f64) -> Result<u64, VulcanError> {
    ui_to_base_units(sol, 9, "SOL")
}

pub fn ui_to_base_units(amount: f64, decimals: u8, label: &str) -> Result<u64, VulcanError> {
    if !amount.is_finite() || amount < 0.0 {
        return Err(VulcanError::validation(
            "SURFNET_INVALID_AMOUNT",
            format!("{label} amount must be a non-negative number, got {amount}"),
        ));
    }
    let scaled = amount * 10f64.powi(decimals as i32);
    if scaled > u64::MAX as f64 {
        return Err(VulcanError::validation(
            "SURFNET_INVALID_AMOUNT",
            format!("{label} amount {amount} overflows base units"),
        ));
    }
    Ok(scaled.round() as u64)
}

/// Parse `90s`, `30m`, `2h`, `1d`, or combinations like `1h30m` into seconds.
pub fn parse_forward_duration(input: &str) -> Result<i64, VulcanError> {
    let s = input.trim();
    let invalid = || {
        VulcanError::validation(
            "SURFNET_INVALID_DURATION",
            format!("Cannot parse duration '{input}'. Use forms like 90s, 30m, 2h, 1d, or 1h30m."),
        )
    };
    if s.is_empty() {
        return Err(invalid());
    }
    let mut total: i64 = 0;
    let mut number = String::new();
    let mut saw_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let multiplier = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(invalid()),
        };
        let value: i64 = number.parse().map_err(|_| invalid())?;
        total = total
            .checked_add(value.checked_mul(multiplier).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        number.clear();
        saw_unit = true;
    }
    if !number.is_empty() {
        // Bare number: seconds.
        total = total
            .checked_add(number.parse().map_err(|_| invalid())?)
            .ok_or_else(invalid)?;
        saw_unit = true;
    }
    if !saw_unit || total <= 0 {
        return Err(invalid());
    }
    Ok(total)
}

/// Surfpool wraps some cheatcode results in `{ context, value }`; return the value.
pub fn unwrap_rpc_value(result: Value) -> Value {
    match result {
        Value::Object(mut map) if map.contains_key("value") && map.contains_key("context") => {
            map.remove("value").unwrap_or(Value::Null)
        }
        other => other,
    }
}

/// Decode the Clock sysvar: slot (u64 LE at 0) and unix_timestamp (i64 LE at 32).
pub fn clock_from_sysvar_bytes(bytes: &[u8]) -> Result<(u64, i64), VulcanError> {
    if bytes.len() < 40 {
        return Err(VulcanError::api(
            "SURFNET_RPC_ERROR",
            format!("clock sysvar has {} bytes, expected 40", bytes.len()),
        ));
    }
    let slot = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let unix_timestamp = i64::from_le_bytes(bytes[32..40].try_into().unwrap());
    Ok((slot, unix_timestamp))
}

/// Build the `surfpool start` argument list for `StartArgs` (pure, testable).
pub fn surfpool_start_args(
    args: &StartArgs,
    airdrop_pubkeys: &[Pubkey],
    airdrop_lamports: u64,
    manifest_dir: &Path,
) -> Vec<String> {
    let mut out = vec![
        "start".to_string(),
        "--no-tui".to_string(),
        "--no-studio".to_string(),
        "--no-deploy".to_string(),
        "--port".to_string(),
        args.port.to_string(),
        "--ws-port".to_string(),
        args.ws_port.to_string(),
        "--manifest-file-path".to_string(),
        manifest_dir.join("txtx.yml").display().to_string(),
        "--log-path".to_string(),
        manifest_dir.join("surfpool-logs").display().to_string(),
    ];
    match &args.rpc_url {
        Some(url) => out.extend(["--rpc-url".to_string(), url.clone()]),
        None => out.extend(["--network".to_string(), args.network.as_str().to_string()]),
    }
    for path in &args.snapshot {
        out.extend(["--snapshot".to_string(), path.display().to_string()]);
    }
    // Surfpool airdrops to ~/.config/solana/id.json by default; opt out and only
    // fund what the user asked for.
    if airdrop_pubkeys.is_empty() {
        out.extend(["--airdrop-amount".to_string(), "0".to_string()]);
    } else {
        out.extend(["--airdrop-amount".to_string(), airdrop_lamports.to_string()]);
        for key in airdrop_pubkeys {
            out.extend(["--airdrop".to_string(), key.to_string()]);
        }
    }
    if let Some(ms) = args.slot_time_ms {
        out.extend(["--slot-time".to_string(), ms.to_string()]);
    }
    if args.skip_signature_verification {
        out.push("--skip-signature-verification".to_string());
    }
    out.extend(args.surfpool_args.iter().cloned());
    out
}

// ── Scenario files ──────────────────────────────────────────────────────

/// Declarative Surfnet setup. See `vulcan surfnet scenario example`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<ScenarioNetwork>,
    #[serde(default, rename = "account", skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountOverride>,
    #[serde(default, rename = "fund", skip_serializing_if = "Vec::is_empty")]
    pub funding: Vec<FundStep>,
    #[serde(default, rename = "runbook", skip_serializing_if = "Vec::is_empty")]
    pub runbooks: Vec<RunbookStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockStep>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioNetwork {
    /// `mainnet` (default), `devnet`, or `testnet`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// Datasource RPC URL override (takes precedence over `network`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Snapshot files (relative to the scenario file) to preload
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_time_ms: Option<u64>,
    #[serde(default)]
    pub skip_signature_verification: bool,
}

/// Raw `surfnet_setAccount` override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountOverride {
    pub pubkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lamports: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Base64-encoded account data
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<bool>,
}

impl AccountOverride {
    pub fn to_update(&self) -> Value {
        let mut update = serde_json::Map::new();
        if let Some(l) = self.lamports {
            update.insert("lamports".into(), json!(l));
        }
        if let Some(o) = &self.owner {
            update.insert("owner".into(), json!(o));
        }
        if let Some(d) = &self.data_base64 {
            update.insert("data".into(), json!(d));
        }
        if let Some(e) = self.executable {
            update.insert("executable".into(), json!(e));
        }
        Value::Object(update)
    }
}

/// Fund a stored wallet (by name) or a pubkey with SOL and/or a token balance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sol: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usdc: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
}

/// A txtx runbook executed with `surfpool run` against the Surfnet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunbookStep {
    /// Runbook ID from the manifest, or a path to a `.tx` file
    pub id: String,
    /// Manifest path (relative to the scenario file); defaults to `txtx.yml`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub travel_to_slot: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub travel_to_timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub travel_forward: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<bool>,
}

pub const EXAMPLE_SCENARIO: &str = r#"# Vulcan Surfnet scenario — apply with `vulcan surfnet scenario run <file> --start`
name = "funded-trader"
description = "Fork mainnet, fund the default trader wallet and a paymaster, then freeze the clock."

[network]
network = "mainnet"          # mainnet | devnet | testnet, or set rpc_url
port = 8899
snapshots = []               # JSON files from `vulcan surfnet snapshot --out ...`
skip_signature_verification = false

# Fund stored wallets by name (or any pubkey) with SOL and USDC.
[[fund]]
wallet = "trader"
sol = 5
usdc = 10000

[[fund]]
wallet = "sponsor"
sol = 50

# Raw account overrides (surfnet_setAccount). Useful for patching oracles or
# cloning a whale's trader account. Data is base64.
# [[account]]
# pubkey = "..."
# lamports = 1000000000
# owner = "..."
# data_base64 = "..."

# txtx runbooks executed with `surfpool run` after funding (optional).
# [[runbook]]
# id = "seed"
# manifest = "txtx.yml"
# inputs = ["inputs.json"]

[clock]
travel_forward = "1h"        # or travel_to_slot / travel_to_timestamp
pause = true
"#;

pub fn load_scenario(path: &Path) -> Result<Scenario, VulcanError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        VulcanError::io(
            "SCENARIO_READ_FAILED",
            format!("Cannot read scenario {}: {e}", path.display()),
        )
    })?;
    parse_scenario(&raw)
}

pub fn parse_scenario(raw: &str) -> Result<Scenario, VulcanError> {
    let scenario: Scenario = toml::from_str(raw)
        .map_err(|e| VulcanError::validation("SCENARIO_INVALID", e.to_string()))?;
    validate_scenario(&scenario)?;
    Ok(scenario)
}

fn validate_scenario(s: &Scenario) -> Result<(), VulcanError> {
    if s.name.trim().is_empty() {
        return Err(VulcanError::validation(
            "SCENARIO_INVALID",
            "`name` is required",
        ));
    }
    for (i, f) in s.funding.iter().enumerate() {
        if f.wallet.is_none() == f.pubkey.is_none() {
            return Err(VulcanError::validation(
                "SCENARIO_INVALID",
                format!(
                    "[[fund]] #{}: set exactly one of `wallet` or `pubkey`",
                    i + 1
                ),
            ));
        }
        if f.sol.is_none() && f.usdc.is_none() {
            return Err(VulcanError::validation(
                "SCENARIO_INVALID",
                format!("[[fund]] #{}: set `sol` and/or `usdc`", i + 1),
            ));
        }
        if let Some(p) = &f.pubkey {
            Pubkey::from_str(p).map_err(|e| {
                VulcanError::validation("SCENARIO_INVALID", format!("[[fund]] #{}: {e}", i + 1))
            })?;
        }
    }
    for (i, a) in s.accounts.iter().enumerate() {
        Pubkey::from_str(&a.pubkey).map_err(|e| {
            VulcanError::validation("SCENARIO_INVALID", format!("[[account]] #{}: {e}", i + 1))
        })?;
        if a.lamports.is_none()
            && a.owner.is_none()
            && a.data_base64.is_none()
            && a.executable.is_none()
        {
            return Err(VulcanError::validation(
                "SCENARIO_INVALID",
                format!("[[account]] #{}: nothing to override", i + 1),
            ));
        }
    }
    if let Some(c) = &s.clock {
        let targets = [
            c.travel_to_slot.is_some(),
            c.travel_to_timestamp.is_some(),
            c.travel_forward.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if targets > 1 {
            return Err(VulcanError::validation(
                "SCENARIO_INVALID",
                "[clock]: use only one of travel_to_slot, travel_to_timestamp, travel_forward",
            ));
        }
        if let Some(f) = &c.travel_forward {
            parse_forward_duration(f)?;
        }
    }
    if let Some(n) = &s.network {
        if let Some(net) = &n.network {
            if !matches!(net.as_str(), "mainnet" | "devnet" | "testnet") {
                return Err(VulcanError::validation(
                    "SCENARIO_INVALID",
                    format!("[network].network must be mainnet, devnet, or testnet, got '{net}'"),
                ));
            }
        }
    }
    Ok(())
}

// ── Result types ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SurfnetStarted {
    pub rpc_url: String,
    pub ws_url: String,
    pub network: String,
    pub pid: Option<u32>,
    pub slot: u64,
    pub log_path: PathBuf,
    pub skip_signature_verification: bool,
}

impl TableRenderable for SurfnetStarted {
    fn render_table(&self) {
        println!(
            "Surfnet running ({} fork) at {}",
            self.network, self.rpc_url
        );
        println!("  WebSocket: {}", self.ws_url);
        println!("  Slot:      {}", self.slot);
        if let Some(pid) = self.pid {
            println!("  PID:       {pid}");
        }
        println!("  Logs:      {}", self.log_path.display());
        if self.skip_signature_verification {
            println!("  Signature verification is OFF: any pubkey can sign on this fork.");
        }
        println!(
            "Target it from any command with `--surfnet`, e.g. `vulcan --surfnet wallet balance`."
        );
    }
}

#[derive(Debug, Serialize)]
pub struct SurfnetStopped {
    pub rpc_url: String,
    pub pid: Option<u32>,
    pub killed: bool,
}

impl TableRenderable for SurfnetStopped {
    fn render_table(&self) {
        if self.killed {
            println!("Surfnet at {} stopped.", self.rpc_url);
        } else {
            println!(
                "No Surfnet process to stop; cleared stale state for {}.",
                self.rpc_url
            );
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SurfnetStatus {
    pub running: bool,
    pub rpc_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_unix_timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runbook_executions: Option<Value>,
}

impl TableRenderable for SurfnetStatus {
    fn render_table(&self) {
        if !self.running {
            println!(
                "No Surfnet running at {}. Start one with `vulcan surfnet start`.",
                self.rpc_url
            );
            return;
        }
        println!("Surfnet running at {}", self.rpc_url);
        if let Some(n) = &self.network {
            println!("  Network:  {n} fork");
        }
        if let Some(s) = self.slot {
            println!("  Slot:     {s}");
        }
        if let (Some(ts), Some(utc)) = (self.clock_unix_timestamp, &self.clock_utc) {
            println!("  Clock:    {utc} ({ts})");
        }
        if let Some(pid) = self.pid {
            println!("  PID:      {pid}");
        }
        if let Some(at) = &self.started_at {
            println!("  Started:  {at}");
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SurfnetFunded {
    pub pubkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sol: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,
}

impl TableRenderable for SurfnetFunded {
    fn render_table(&self) {
        let who = match &self.wallet {
            Some(w) => format!("{w} ({})", self.pubkey),
            None => self.pubkey.clone(),
        };
        println!("Funded {who} on the Surfnet:");
        if let Some(sol) = self.sol {
            println!("  SOL:   {sol}");
        }
        if let (Some(amount), Some(mint)) = (self.token_amount, &self.mint) {
            println!("  Token: {amount} ({mint})");
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ClockChanged {
    pub action: String,
    pub slot: u64,
    pub clock_unix_timestamp: i64,
    pub clock_utc: String,
}

impl TableRenderable for ClockChanged {
    fn render_table(&self) {
        println!(
            "Clock {}: slot {}, {} ({})",
            self.action, self.slot, self.clock_utc, self.clock_unix_timestamp
        );
    }
}

#[derive(Debug, Serialize)]
pub struct SnapshotExported {
    pub path: PathBuf,
    pub accounts: usize,
    pub bytes: usize,
}

impl TableRenderable for SnapshotExported {
    fn render_table(&self) {
        println!(
            "Exported {} accounts ({} bytes) to {}",
            self.accounts,
            self.bytes,
            self.path.display()
        );
        println!(
            "Reload with `vulcan surfnet start --snapshot {}`.",
            self.path.display()
        );
    }
}

#[derive(Debug, Serialize)]
pub struct SurfnetReset {
    pub slot: u64,
}

impl TableRenderable for SurfnetReset {
    fn render_table(&self) {
        println!("Surfnet reset to its initial state (slot {}).", self.slot);
    }
}

#[derive(Debug, Serialize)]
pub struct ScenarioSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub steps: Vec<String>,
}

impl TableRenderable for ScenarioSummary {
    fn render_table(&self) {
        println!("Scenario '{}'", self.name);
        if let Some(d) = &self.description {
            println!("  {d}");
        }
        for (i, step) in self.steps.iter().enumerate() {
            println!("  {}. {step}", i + 1);
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ScenarioApplied {
    pub name: String,
    pub rpc_url: String,
    pub applied: Vec<String>,
    pub slot: u64,
}

impl TableRenderable for ScenarioApplied {
    fn render_table(&self) {
        println!(
            "Scenario '{}' applied to {} (slot {}):",
            self.name, self.rpc_url, self.slot
        );
        for step in &self.applied {
            println!("  - {step}");
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ScenarioExample {
    pub toml: String,
}

impl TableRenderable for ScenarioExample {
    fn render_table(&self) {
        print!("{}", self.toml);
    }
}

// ── Execution ───────────────────────────────────────────────────────────

pub async fn execute(ctx: &AppContext, cmd: SurfnetCommand) -> Result<(), VulcanError> {
    match cmd {
        SurfnetCommand::Start(args) => {
            let result = start(ctx, &args).await?;
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Stop => {
            let result = stop(ctx).await?;
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Status => {
            let result = status(ctx).await?;
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Fund(args) => {
            let rpc = connected_rpc(ctx).await?;
            let (pubkey, wallet) = resolve_target(ctx, &args.target)?;
            let result = fund(
                &rpc,
                pubkey,
                wallet,
                args.sol,
                args.usdc,
                args.mint.as_deref(),
                args.decimals,
            )
            .await?;
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::TimeTravel(args) => {
            let rpc = connected_rpc(ctx).await?;
            let target = time_travel_target(&rpc, &args).await?;
            rpc.time_travel(target).await?;
            let result = clock_result(&rpc, "moved").await?;
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Clock { action } => {
            let rpc = connected_rpc(ctx).await?;
            let label = match action {
                ClockAction::Pause => {
                    rpc.pause_clock().await?;
                    "paused"
                }
                ClockAction::Resume => {
                    rpc.resume_clock().await?;
                    "resumed"
                }
            };
            let result = clock_result(&rpc, label).await?;
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Snapshot { out } => {
            let rpc = connected_rpc(ctx).await?;
            let snapshot = rpc.export_snapshot().await?;
            let accounts = snapshot.as_object().map(|m| m.len()).unwrap_or(0);
            let raw = serde_json::to_string(&snapshot)
                .map_err(|e| VulcanError::internal("SNAPSHOT_ENCODE_FAILED", e.to_string()))?;
            std::fs::write(&out, &raw)
                .map_err(|e| VulcanError::io("SNAPSHOT_WRITE_FAILED", e.to_string()))?;
            let result = SnapshotExported {
                path: out,
                accounts,
                bytes: raw.len(),
            };
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Reset => {
            let rpc = connected_rpc(ctx).await?;
            rpc.reset_network().await?;
            let result = SurfnetReset {
                slot: rpc.slot().await?,
            };
            render_success(ctx.output_format, &result, Value::Null);
            Ok(())
        }
        SurfnetCommand::Scenario(cmd) => match cmd {
            ScenarioCommand::Example => {
                let result = ScenarioExample {
                    toml: EXAMPLE_SCENARIO.to_string(),
                };
                render_success(ctx.output_format, &result, Value::Null);
                Ok(())
            }
            ScenarioCommand::Validate { file } => {
                let scenario = load_scenario(&file)?;
                let result = summarize_scenario(&scenario);
                render_success(ctx.output_format, &result, Value::Null);
                Ok(())
            }
            ScenarioCommand::Run { file, start } => {
                let scenario = load_scenario(&file)?;
                let base_dir = file
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let result = run_scenario(ctx, &scenario, &base_dir, start).await?;
                render_success(ctx.output_format, &result, Value::Null);
                Ok(())
            }
        },
    }
}

fn surfnet_rpc(ctx: &AppContext, url: String) -> SurfnetRpc {
    SurfnetRpc::new(url, ctx.raw_http_client.clone())
}

/// RPC for the Surfnet this Vulcan started (or the default URL), verified healthy.
async fn connected_rpc(ctx: &AppContext) -> Result<SurfnetRpc, VulcanError> {
    let url = surfnet_rpc_url(&ctx.vulcan_dir);
    let rpc = surfnet_rpc(ctx, url);
    if !rpc.is_healthy().await {
        return Err(VulcanError::network(
            "SURFNET_NOT_RUNNING",
            format!(
                "No healthy Surfnet at {}. Start one with `vulcan surfnet start`.",
                rpc.url()
            ),
        ));
    }
    Ok(rpc)
}

fn resolve_target(ctx: &AppContext, target: &str) -> Result<(Pubkey, Option<String>), VulcanError> {
    let target = target.trim();
    if ctx.wallet_store.exists(target) {
        let file = ctx
            .wallet_store
            .load(target)
            .map_err(|e| VulcanError::auth("WALLET_NOT_FOUND", e.to_string()))?;
        let pubkey = Pubkey::from_str(&file.public_key)
            .map_err(|e| VulcanError::validation("INVALID_PUBKEY", e.to_string()))?;
        return Ok((pubkey, Some(target.to_string())));
    }
    Pubkey::from_str(target)
        .map(|p| (p, None))
        .map_err(|_| {
            VulcanError::validation(
                "SURFNET_INVALID_TARGET",
                format!("'{target}' is neither a stored wallet name nor a valid pubkey. Use `vulcan wallet list`."),
            )
        })
}

fn ensure_surfpool_installed() -> Result<String, VulcanError> {
    let out = Command::new("surfpool").arg("--version").output().map_err(|_| {
        VulcanError::config(
            "SURFPOOL_NOT_INSTALLED",
            "`surfpool` is not on PATH. Install it with `curl -sL https://run.surfpool.run/ | bash` (or `brew install txtx/taps/surfpool`).",
        )
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn start(ctx: &AppContext, args: &StartArgs) -> Result<SurfnetStarted, VulcanError> {
    ensure_surfpool_installed()?;
    let rpc_url = format!("http://127.0.0.1:{}", args.port);
    let ws_url = format!("ws://127.0.0.1:{}", args.ws_port);

    if let Some(existing) = load_state(&ctx.vulcan_dir)? {
        if surfnet_rpc(ctx, existing.rpc_url.clone())
            .await_healthy_once()
            .await
        {
            return Err(VulcanError::validation(
                "SURFNET_ALREADY_RUNNING",
                format!(
                    "A Surfnet is already running at {} (pid {:?}). Stop it with `vulcan surfnet stop` first.",
                    existing.rpc_url, existing.pid
                ),
            ));
        }
        clear_state(&ctx.vulcan_dir)?;
    } else if surfnet_rpc(ctx, rpc_url.clone()).await_healthy_once().await {
        return Err(VulcanError::validation(
            "SURFNET_PORT_IN_USE",
            format!("Something already answers RPC at {rpc_url}. Pick another --port or stop it."),
        ));
    }

    let mut airdrop_pubkeys = Vec::new();
    for target in &args.airdrop {
        airdrop_pubkeys.push(resolve_target(ctx, target)?.0);
    }
    let airdrop_lamports = sol_to_lamports(args.airdrop_sol)?;
    let work_dir = ctx.vulcan_dir.join("surfnet");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| VulcanError::io("SURFNET_STATE_WRITE_FAILED", e.to_string()))?;
    let log_path = ctx.vulcan_dir.join("surfnet.log");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| VulcanError::io("SURFNET_STATE_WRITE_FAILED", e.to_string()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| VulcanError::io("SURFNET_STATE_WRITE_FAILED", e.to_string()))?;

    let cli_args = surfpool_start_args(args, &airdrop_pubkeys, airdrop_lamports, &work_dir);
    let mut command = Command::new("surfpool");
    command
        .args(&cli_args)
        .current_dir(&work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: survives this CLI exiting and terminal hangups.
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|e| VulcanError::io("SURFNET_START_FAILED", format!("spawning surfpool: {e}")))?;
    let pid = child.id();

    let rpc = surfnet_rpc(ctx, rpc_url.clone());
    let deadline = Instant::now() + Duration::from_secs(args.wait_secs);
    let mut healthy = false;
    while Instant::now() < deadline {
        if rpc.is_healthy().await {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !healthy {
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let tail = std::fs::read_to_string(&log_path)
            .map(|s| {
                s.lines()
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        return Err(VulcanError::network(
            "SURFNET_START_TIMEOUT",
            format!(
                "Surfnet did not become healthy within {}s. Last log lines:\n{tail}",
                args.wait_secs
            ),
        ));
    }

    let state = SurfnetState {
        rpc_url: rpc_url.clone(),
        ws_url: ws_url.clone(),
        network: args
            .rpc_url
            .clone()
            .unwrap_or_else(|| args.network.as_str().to_string()),
        datasource_rpc_url: args.rpc_url.clone(),
        pid: Some(pid),
        started_at: chrono::Utc::now().to_rfc3339(),
        log_path: log_path.clone(),
        skip_signature_verification: args.skip_signature_verification,
    };
    save_state(&ctx.vulcan_dir, &state)?;

    Ok(SurfnetStarted {
        rpc_url,
        ws_url,
        network: state.network,
        pid: Some(pid),
        slot: rpc.slot().await?,
        log_path,
        skip_signature_verification: args.skip_signature_verification,
    })
}

impl SurfnetRpc {
    async fn await_healthy_once(&self) -> bool {
        self.is_healthy().await
    }
}

async fn stop(ctx: &AppContext) -> Result<SurfnetStopped, VulcanError> {
    let Some(state) = load_state(&ctx.vulcan_dir)? else {
        return Err(VulcanError::validation(
            "SURFNET_NOT_RUNNING",
            "No Surfnet was started through Vulcan (no ~/.vulcan/surfnet.json).",
        ));
    };
    let mut killed = false;
    if let Some(pid) = state.pid {
        killed = Command::new("kill")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if killed {
            let rpc = surfnet_rpc(ctx, state.rpc_url.clone());
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && rpc.is_healthy().await {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    clear_state(&ctx.vulcan_dir)?;
    Ok(SurfnetStopped {
        rpc_url: state.rpc_url,
        pid: state.pid,
        killed,
    })
}

async fn status(ctx: &AppContext) -> Result<SurfnetStatus, VulcanError> {
    let state = load_state(&ctx.vulcan_dir)?;
    let rpc_url = state
        .as_ref()
        .map(|s| s.rpc_url.clone())
        .unwrap_or_else(|| DEFAULT_SURFNET_RPC_URL.to_string());
    let rpc = surfnet_rpc(ctx, rpc_url.clone());
    if !rpc.is_healthy().await {
        return Ok(SurfnetStatus {
            running: false,
            rpc_url,
            network: state.as_ref().map(|s| s.network.clone()),
            pid: state.as_ref().and_then(|s| s.pid),
            started_at: state.map(|s| s.started_at),
            slot: None,
            clock_unix_timestamp: None,
            clock_utc: None,
            runbook_executions: None,
        });
    }
    let (slot, ts) = rpc.clock().await?;
    let info = rpc.info().await.ok();
    Ok(SurfnetStatus {
        running: true,
        rpc_url,
        network: state.as_ref().map(|s| s.network.clone()),
        pid: state.as_ref().and_then(|s| s.pid),
        started_at: state.map(|s| s.started_at),
        slot: Some(slot),
        clock_unix_timestamp: Some(ts),
        clock_utc: Some(format_utc(ts)),
        runbook_executions: info.and_then(|i| {
            i["value"]["runbookExecutions"]
                .as_array()
                .cloned()
                .map(Value::Array)
        }),
    })
}

fn format_utc(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

async fn fund(
    rpc: &SurfnetRpc,
    pubkey: Pubkey,
    wallet: Option<String>,
    sol: Option<f64>,
    token: Option<f64>,
    mint: Option<&str>,
    decimals: u8,
) -> Result<SurfnetFunded, VulcanError> {
    if sol.is_none() && token.is_none() {
        return Err(VulcanError::validation(
            "SURFNET_NOTHING_TO_FUND",
            "Pass --sol and/or --usdc.",
        ));
    }
    if let Some(sol) = sol {
        rpc.set_lamports(&pubkey, sol_to_lamports(sol)?).await?;
    }
    let mut mint_used = None;
    if let Some(amount) = token {
        let mint_str = mint.unwrap_or(USDC_MINT);
        let mint_key = Pubkey::from_str(mint_str)
            .map_err(|e| VulcanError::validation("INVALID_PUBKEY", format!("mint: {e}")))?;
        rpc.set_token_amount(
            &pubkey,
            &mint_key,
            ui_to_base_units(amount, decimals, "token")?,
        )
        .await?;
        mint_used = Some(mint_str.to_string());
    }
    Ok(SurfnetFunded {
        pubkey: pubkey.to_string(),
        wallet,
        sol,
        token_amount: token,
        mint: mint_used,
    })
}

async fn time_travel_target(
    rpc: &SurfnetRpc,
    args: &TimeTravelArgs,
) -> Result<TimeTravelTarget, VulcanError> {
    if let Some(slot) = args.slot {
        return Ok(TimeTravelTarget::Slot(slot));
    }
    if let Some(ts) = args.timestamp {
        return Ok(TimeTravelTarget::Timestamp(ts));
    }
    let forward = args.forward.as_deref().ok_or_else(|| {
        VulcanError::validation("SURFNET_INVALID_DURATION", "no time-travel target")
    })?;
    let (_, now) = rpc.clock().await?;
    Ok(TimeTravelTarget::Timestamp(
        now + parse_forward_duration(forward)?,
    ))
}

async fn clock_result(rpc: &SurfnetRpc, action: &str) -> Result<ClockChanged, VulcanError> {
    // The Clock sysvar's slot can lag right after a jump; report the bank slot.
    let slot = rpc.slot().await?;
    let (_, ts) = rpc.clock().await?;
    Ok(ClockChanged {
        action: action.to_string(),
        slot,
        clock_unix_timestamp: ts,
        clock_utc: format_utc(ts),
    })
}

fn summarize_scenario(s: &Scenario) -> ScenarioSummary {
    let mut steps = Vec::new();
    if let Some(n) = &s.network {
        let source = n
            .rpc_url
            .clone()
            .unwrap_or_else(|| n.network.clone().unwrap_or_else(|| "mainnet".into()));
        let mut line = format!(
            "start Surfnet forking {source} on port {}",
            n.port.unwrap_or(8899)
        );
        if !n.snapshots.is_empty() {
            line.push_str(&format!(" with {} snapshot file(s)", n.snapshots.len()));
        }
        if n.skip_signature_verification {
            line.push_str(", signature verification off");
        }
        steps.push(line + " (only with --start)");
    }
    for a in &s.accounts {
        steps.push(format!("override account {}", a.pubkey));
    }
    for f in &s.funding {
        let who = f
            .wallet
            .clone()
            .or_else(|| f.pubkey.clone())
            .unwrap_or_default();
        let mut parts = Vec::new();
        if let Some(sol) = f.sol {
            parts.push(format!("{sol} SOL"));
        }
        if let Some(t) = f.usdc {
            parts.push(format!(
                "{t} {}",
                f.mint.as_deref().map(|_| "token").unwrap_or("USDC")
            ));
        }
        steps.push(format!("fund {who} with {}", parts.join(" and ")));
    }
    for r in &s.runbooks {
        steps.push(format!("run txtx runbook '{}'", r.id));
    }
    if let Some(c) = &s.clock {
        if let Some(slot) = c.travel_to_slot {
            steps.push(format!("time-travel to slot {slot}"));
        }
        if let Some(ts) = c.travel_to_timestamp {
            steps.push(format!("time-travel to {}", format_utc(ts)));
        }
        if let Some(f) = &c.travel_forward {
            steps.push(format!("time-travel forward {f}"));
        }
        if c.pause == Some(true) {
            steps.push("pause the clock".into());
        }
        if c.pause == Some(false) {
            steps.push("resume the clock".into());
        }
    }
    ScenarioSummary {
        name: s.name.clone(),
        description: s.description.clone(),
        steps,
    }
}

async fn run_scenario(
    ctx: &AppContext,
    scenario: &Scenario,
    base_dir: &Path,
    start_first: bool,
) -> Result<ScenarioApplied, VulcanError> {
    let mut applied = Vec::new();
    if start_first {
        let net = scenario.network.clone().unwrap_or_default();
        let network = match net.network.as_deref() {
            None | Some("mainnet") => ForkNetwork::Mainnet,
            Some("devnet") => ForkNetwork::Devnet,
            Some("testnet") => ForkNetwork::Testnet,
            Some(other) => {
                return Err(VulcanError::validation(
                    "SCENARIO_INVALID",
                    format!("unknown network '{other}'"),
                ))
            }
        };
        let args = StartArgs {
            port: net.port.unwrap_or(8899),
            ws_port: net.port.map(|p| p.saturating_add(1)).unwrap_or(8900),
            network,
            rpc_url: net.rpc_url.clone(),
            snapshot: net.snapshots.iter().map(|p| base_dir.join(p)).collect(),
            airdrop: Vec::new(),
            airdrop_sol: 0.0,
            slot_time_ms: net.slot_time_ms,
            skip_signature_verification: net.skip_signature_verification,
            wait_secs: 90,
            surfpool_args: Vec::new(),
        };
        let started = start(ctx, &args).await?;
        applied.push(format!("started Surfnet at {}", started.rpc_url));
    }

    let rpc = connected_rpc(ctx).await?;
    for a in &scenario.accounts {
        let pubkey = Pubkey::from_str(&a.pubkey)
            .map_err(|e| VulcanError::validation("SCENARIO_INVALID", e.to_string()))?;
        rpc.set_account(&pubkey, a.to_update()).await?;
        applied.push(format!("overrode account {pubkey}"));
    }
    for f in &scenario.funding {
        let (pubkey, wallet) = match (&f.wallet, &f.pubkey) {
            (Some(w), _) => resolve_target(ctx, w)?,
            (None, Some(p)) => resolve_target(ctx, p)?,
            (None, None) => unreachable!("validated"),
        };
        let funded = fund(
            &rpc,
            pubkey,
            wallet,
            f.sol,
            f.usdc,
            f.mint.as_deref(),
            f.decimals.unwrap_or(6),
        )
        .await?;
        applied.push(format!(
            "funded {}{}{}",
            funded.wallet.as_deref().unwrap_or(&funded.pubkey),
            funded.sol.map(|s| format!(" sol={s}")).unwrap_or_default(),
            funded
                .token_amount
                .map(|t| format!(" token={t}"))
                .unwrap_or_default(),
        ));
    }
    for r in &scenario.runbooks {
        run_runbook(r, base_dir, rpc.url())?;
        applied.push(format!("ran runbook '{}'", r.id));
    }
    if let Some(c) = &scenario.clock {
        if let Some(slot) = c.travel_to_slot {
            rpc.time_travel(TimeTravelTarget::Slot(slot)).await?;
            applied.push(format!("time-travelled to slot {slot}"));
        }
        if let Some(ts) = c.travel_to_timestamp {
            rpc.time_travel(TimeTravelTarget::Timestamp(ts)).await?;
            applied.push(format!("time-travelled to {}", format_utc(ts)));
        }
        if let Some(f) = &c.travel_forward {
            let (_, now) = rpc.clock().await?;
            let secs = parse_forward_duration(f)?;
            rpc.time_travel(TimeTravelTarget::Timestamp(now + secs))
                .await?;
            applied.push(format!("time-travelled forward {f}"));
        }
        match c.pause {
            Some(true) => {
                rpc.pause_clock().await?;
                applied.push("paused the clock".into());
            }
            Some(false) => {
                rpc.resume_clock().await?;
                applied.push("resumed the clock".into());
            }
            None => {}
        }
    }
    Ok(ScenarioApplied {
        name: scenario.name.clone(),
        rpc_url: rpc.url().to_string(),
        applied,
        slot: rpc.slot().await?,
    })
}

fn run_runbook(step: &RunbookStep, base_dir: &Path, rpc_url: &str) -> Result<(), VulcanError> {
    ensure_surfpool_installed()?;
    let manifest = base_dir.join(
        step.manifest
            .clone()
            .unwrap_or_else(|| PathBuf::from("txtx.yml")),
    );
    let mut command = Command::new("surfpool");
    command
        .arg("run")
        .arg(&step.id)
        .arg("--unsupervised")
        .arg("--manifest-file-path")
        .arg(&manifest)
        .current_dir(base_dir)
        .env("SURFPOOL_RPC_URL", rpc_url);
    if let Some(env) = &step.env {
        command.arg("--env").arg(env);
    }
    for input in &step.inputs {
        command.arg("--input").arg(base_dir.join(input));
    }
    let status = command
        .status()
        .map_err(|e| VulcanError::io("RUNBOOK_SPAWN_FAILED", e.to_string()))?;
    if !status.success() {
        return Err(VulcanError::tx_failed(
            "RUNBOOK_FAILED",
            format!("runbook '{}' exited with {status}", step.id),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_durations_parse() {
        assert_eq!(parse_forward_duration("90s").unwrap(), 90);
        assert_eq!(parse_forward_duration("30m").unwrap(), 1_800);
        assert_eq!(parse_forward_duration("2h").unwrap(), 7_200);
        assert_eq!(parse_forward_duration("1d").unwrap(), 86_400);
        assert_eq!(parse_forward_duration("1h30m").unwrap(), 5_400);
        assert_eq!(parse_forward_duration("45").unwrap(), 45);
        for bad in ["", "abc", "1x", "-5m", "0s"] {
            assert!(parse_forward_duration(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn amounts_convert_to_base_units() {
        assert_eq!(sol_to_lamports(1.5).unwrap(), 1_500_000_000);
        assert_eq!(
            ui_to_base_units(10_000.0, 6, "USDC").unwrap(),
            10_000_000_000
        );
        assert_eq!(ui_to_base_units(0.000001, 6, "USDC").unwrap(), 1);
        assert!(sol_to_lamports(-1.0).is_err());
        assert!(sol_to_lamports(f64::NAN).is_err());
    }

    #[test]
    fn time_travel_timestamp_is_sent_in_milliseconds() {
        assert_eq!(
            TimeTravelTarget::Timestamp(1_700_000_000).to_param(),
            json!({ "absoluteTimestamp": 1_700_000_000_000i64 })
        );
        assert_eq!(
            TimeTravelTarget::Slot(42).to_param(),
            json!({ "absoluteSlot": 42 })
        );
    }

    #[test]
    fn cheatcode_params_match_surfpool_shapes() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        assert_eq!(
            set_account_params(&owner, json!({ "lamports": 5 })),
            json!([owner.to_string(), { "lamports": 5 }])
        );
        assert_eq!(
            set_token_account_params(&owner, &mint, 7),
            json!([owner.to_string(), mint.to_string(), { "amount": 7 }])
        );
    }

    #[test]
    fn snapshot_export_unwraps_rpc_context_envelope() {
        let wrapped = json!({ "context": { "slot": 1 }, "value": { "Abc": { "lamports": 5 } } });
        assert_eq!(
            unwrap_rpc_value(wrapped),
            json!({ "Abc": { "lamports": 5 } })
        );
        let bare = json!({ "Abc": { "lamports": 5 } });
        assert_eq!(unwrap_rpc_value(bare.clone()), bare);
    }

    #[test]
    fn clock_sysvar_decodes_slot_and_timestamp() {
        let mut bytes = [0u8; 40];
        bytes[0..8].copy_from_slice(&123u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        assert_eq!(
            clock_from_sysvar_bytes(&bytes).unwrap(),
            (123, 1_700_000_000)
        );
        assert!(clock_from_sysvar_bytes(&bytes[..10]).is_err());
    }

    #[test]
    fn example_scenario_parses_and_summarizes() {
        let scenario = parse_scenario(EXAMPLE_SCENARIO).expect("example must be valid");
        assert_eq!(scenario.name, "funded-trader");
        assert_eq!(scenario.funding.len(), 2);
        let summary = summarize_scenario(&scenario);
        assert!(summary
            .steps
            .iter()
            .any(|s| s.contains("fund trader with 5 SOL and 10000 USDC")));
        assert!(summary
            .steps
            .iter()
            .any(|s| s.contains("time-travel forward 1h")));
        assert!(summary.steps.iter().any(|s| s == "pause the clock"));
    }

    #[test]
    fn scenario_validation_catches_mistakes() {
        let missing_target = "name = \"x\"\n[[fund]]\nsol = 1\n";
        assert_eq!(
            parse_scenario(missing_target).unwrap_err().code,
            "SCENARIO_INVALID"
        );

        let both_targets = "name = \"x\"\n[[fund]]\nwallet = \"a\"\npubkey = \"b\"\nsol = 1\n";
        assert!(parse_scenario(both_targets).is_err());

        let two_clock_targets =
            "name = \"x\"\n[clock]\ntravel_to_slot = 1\ntravel_forward = \"1h\"\n";
        assert!(parse_scenario(two_clock_targets).is_err());

        let unknown_field = "name = \"x\"\nbogus = 1\n";
        assert!(parse_scenario(unknown_field).is_err());

        let bad_network = "name = \"x\"\n[network]\nnetwork = \"localnet\"\n";
        assert!(parse_scenario(bad_network).is_err());
    }

    #[test]
    fn account_override_builds_only_set_fields() {
        let o = AccountOverride {
            pubkey: Pubkey::new_unique().to_string(),
            lamports: Some(1),
            owner: None,
            data_base64: Some("AA==".into()),
            executable: None,
        };
        assert_eq!(o.to_update(), json!({ "lamports": 1, "data": "AA==" }));
    }

    #[test]
    fn surfpool_args_cover_fork_source_airdrops_and_passthrough() {
        let args = StartArgs {
            port: 9001,
            ws_port: 9002,
            network: ForkNetwork::Devnet,
            rpc_url: None,
            snapshot: vec![PathBuf::from("snap.json")],
            airdrop: vec![],
            airdrop_sol: 1.0,
            slot_time_ms: Some(100),
            skip_signature_verification: true,
            wait_secs: 1,
            surfpool_args: vec!["--offline".into()],
        };
        let key = Pubkey::new_unique();
        let out = surfpool_start_args(&args, &[key], 1_000_000_000, Path::new("/tmp/w"));
        let joined = out.join(" ");
        assert!(
            joined.starts_with("start --no-tui --no-studio --no-deploy --port 9001 --ws-port 9002")
        );
        assert!(joined.contains("--network devnet"));
        assert!(joined.contains("--snapshot snap.json"));
        assert!(joined.contains(&format!("--airdrop-amount 1000000000 --airdrop {key}")));
        assert!(joined.contains("--slot-time 100"));
        assert!(joined.contains("--skip-signature-verification"));
        assert!(joined.ends_with("--offline"));

        let no_airdrop = StartArgs {
            rpc_url: Some("https://rpc.example".into()),
            airdrop: vec![],
            ..args
        };
        let out = surfpool_start_args(&no_airdrop, &[], 0, Path::new("/tmp/w")).join(" ");
        assert!(out.contains("--rpc-url https://rpc.example"));
        assert!(!out.contains("--network"));
        assert!(out.contains("--airdrop-amount 0"));
    }
}
