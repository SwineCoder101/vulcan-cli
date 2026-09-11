//! Surfnet subcommand definitions — a local Surfpool fork of mainnet for
//! scenario testing beyond paper trading.

use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum SurfnetCommand {
    /// Start a local Surfpool fork (mainnet by default) in the background
    Start(StartArgs),

    /// Stop the Surfnet started by `vulcan surfnet start`
    Stop,

    /// Show whether a Surfnet is running, its slot, clock, and endpoints
    Status,

    /// Set SOL and/or token balances on the fork for a wallet name or pubkey
    Fund(FundArgs),

    /// Jump the Surfnet clock to a slot, a unix timestamp, or forward by a duration
    TimeTravel(TimeTravelArgs),

    /// Pause or resume the Surfnet clock
    Clock {
        #[arg(value_enum)]
        action: ClockAction,
    },

    /// Export every account loaded into the Surfnet as a JSON snapshot
    Snapshot {
        /// Output file path
        #[arg(long)]
        out: PathBuf,
    },

    /// Reset the Surfnet to its initial forked state
    Reset,

    /// Scenario files — reproducible fork setups (funding, overrides, clock, runbooks)
    #[command(subcommand)]
    Scenario(ScenarioCommand),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ForkNetwork {
    Mainnet,
    Devnet,
    Testnet,
}

impl ForkNetwork {
    pub fn as_str(self) -> &'static str {
        match self {
            ForkNetwork::Mainnet => "mainnet",
            ForkNetwork::Devnet => "devnet",
            ForkNetwork::Testnet => "testnet",
        }
    }
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// JSON-RPC port for the Surfnet
    #[arg(long, default_value_t = 8899)]
    pub port: u16,

    /// WebSocket port for the Surfnet
    #[arg(long, default_value_t = 8900)]
    pub ws_port: u16,

    /// Predefined network to fork from
    #[arg(long, value_enum, default_value = "mainnet")]
    pub network: ForkNetwork,

    /// Fork from this datasource RPC URL instead of the predefined network
    #[arg(long)]
    pub rpc_url: Option<String>,

    /// Account snapshot JSON files to preload (surfnet_exportSnapshot format)
    #[arg(long)]
    pub snapshot: Vec<PathBuf>,

    /// Stored wallet names or pubkeys to airdrop SOL to at startup
    #[arg(long)]
    pub airdrop: Vec<String>,

    /// SOL granted to each --airdrop target
    #[arg(long, default_value_t = 100.0)]
    pub airdrop_sol: f64,

    /// Slot time in milliseconds (Surfpool default: 400)
    #[arg(long)]
    pub slot_time_ms: Option<u64>,

    /// Skip signature verification so any pubkey can act as a signer on the fork
    #[arg(long)]
    pub skip_signature_verification: bool,

    /// Seconds to wait for the Surfnet to report healthy
    #[arg(long, default_value_t = 90)]
    pub wait_secs: u64,

    /// Extra arguments passed through to `surfpool start` (after `--`)
    #[arg(last = true)]
    pub surfpool_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct FundArgs {
    /// Stored wallet name or base58 pubkey
    pub target: String,

    /// SOL balance to set
    #[arg(long)]
    pub sol: Option<f64>,

    /// Token balance to set (USDC unless --mint is given)
    #[arg(long)]
    pub usdc: Option<f64>,

    /// Token mint for --usdc (defaults to mainnet USDC)
    #[arg(long)]
    pub mint: Option<String>,

    /// Token decimals for --usdc
    #[arg(long, default_value_t = 6)]
    pub decimals: u8,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("target").required(true).multiple(false)))]
pub struct TimeTravelArgs {
    /// Absolute slot to jump to
    #[arg(long, group = "target")]
    pub slot: Option<u64>,

    /// Absolute unix timestamp (seconds) to jump to
    #[arg(long, group = "target")]
    pub timestamp: Option<i64>,

    /// Duration to jump forward, e.g. `90s`, `30m`, `2h`, `1d`, `1h30m`
    #[arg(long, group = "target")]
    pub forward: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ClockAction {
    Pause,
    Resume,
}

#[derive(Debug, Subcommand)]
pub enum ScenarioCommand {
    /// Parse a scenario file and report what it would do
    Validate {
        /// Scenario TOML file
        file: PathBuf,
    },

    /// Apply a scenario to the running Surfnet (or start one with --start)
    Run {
        /// Scenario TOML file
        file: PathBuf,

        /// Start a Surfnet from the scenario's [network] section first
        #[arg(long)]
        start: bool,
    },

    /// Print an example scenario file
    Example,
}
