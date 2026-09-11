# Surfnet Integration (Surfpool × Vulcan)

**Status: draft.** This document describes the first slice of Surfpool support in Vulcan, what it enables today, what it does not, and the roadmap for trading Phoenix on a forked mainnet.

## Why

Vulcan has two execution surfaces: **paper** (local simulation against live prices, nothing on chain) and **live** (real transactions on mainnet). Neither lets you test the actual on-chain path, the wallet flows, or a strategy's behaviour under a specific market state without risking funds.

[Surfpool](https://surfpool.run) runs a local Solana network that forks mainnet by fetching accounts on demand, and exposes `surfnet_*` cheatcode RPCs to patch balances and accounts, jump the clock, and export/import snapshots. Pointing Vulcan at a Surfnet gives a third surface: **real Vulcan transactions, real Phoenix programs, fake money, replayable state.**

## What ships in this slice

A `vulcan surfnet` command group and a global `--surfnet` flag.

| Command | What it does |
| --- | --- |
| `surfnet start` | Spawns `surfpool start` in its own process group (mainnet fork by default), waits for health, records `~/.vulcan/surfnet.json`. Options: `--port`, `--network`, `--rpc-url`, `--snapshot <file>...`, `--airdrop <wallet\|pubkey>...`, `--slot-time-ms`, `--skip-signature-verification`, passthrough after `--`. |
| `surfnet stop` / `status` | Stop the Surfnet Vulcan started; report health, slot, clock (unix + UTC), pid, runbook executions. |
| `surfnet fund <wallet\|pubkey> --sol N --usdc N [--mint --decimals]` | `surfnet_setAccount` + `surfnet_setTokenAccount`. Wallet names resolve through the Vulcan wallet store. |
| `surfnet time-travel --slot N \| --timestamp T \| --forward 1h30m` | `surfnet_timeTravel` (forward durations are computed from the Clock sysvar). |
| `surfnet clock pause\|resume` | Freeze/resume slot and time progression. |
| `surfnet snapshot --out file.json` | `surfnet_exportSnapshot` (network scope). Reload with `surfnet start --snapshot`. |
| `surfnet reset` | `surfnet_resetNetwork`. |
| `surfnet scenario example\|validate\|run [--start]` | Declarative TOML scenarios (below). |
| `vulcan --surfnet <any command>` | Points the RPC at the running Surfnet (or `http://127.0.0.1:8899`). Conflicts with `--rpc-url`. |

Every command emits the standard `{ ok, data }` envelope, so agents can drive it through MCP-style JSON.

### Scenario files

```toml
name = "funded-trader"
description = "Fork mainnet, fund the trader and a paymaster, freeze the clock."

[network]                      # used only by `scenario run --start`
network = "mainnet"            # or rpc_url = "https://..."
port = 8899
snapshots = ["snapshots/pre-crash.json"]
skip_signature_verification = false

[[fund]]                       # wallet (store name) or pubkey; sol and/or usdc
wallet = "trader"
sol = 5
usdc = 10000

[[account]]                    # raw surfnet_setAccount override
pubkey = "..."
lamports = 1000000000
owner = "..."
data_base64 = "..."

[[runbook]]                    # txtx runbooks via `surfpool run --unsupervised`
id = "seed"
manifest = "txtx.yml"
inputs = ["inputs.json"]

[clock]
travel_forward = "1h"          # or travel_to_slot / travel_to_timestamp
pause = true
```

Steps apply in order: network (if `--start`), account overrides, funding, runbooks, clock. Paths are relative to the scenario file. `scenario validate` prints the plan without touching anything. See `scenarios/funded-trader.toml`.

### Verified against Surfpool 1.5.0

Start → fund → `--surfnet wallet balance` (SOL and USDC visible) → time-travel → pause → snapshot → restart with `--snapshot` → scenario run → reset → stop, all through the CLI on a mainnet fork.

## What does not work yet (and why)

Vulcan reads most trader state from the **Phoenix API**, not the chain. The API indexes mainnet, so anything that happens on the fork is invisible to it:

- `vulcan portfolio`, `position list`, `history ...` will show mainnet state, not fork state.
- `account register` goes through `/v1/referral/activate-tx`, which submits to mainnet and needs the API's onboarder co-signature. There is no way to register a *new* trader on the fork through the API.
- Placing orders builds instructions locally and sends them to the configured RPC, so `--surfnet trade ...` does reach the fork, but only for a trader account that already exists on mainnet (and therefore on the fork).
- Market data (`market ticker`, candles) comes from the API and reflects mainnet, which is fine for most scenarios but means oracle/mark prices on the fork drift from the API after time travel.

## Roadmap

1. **Impersonate an existing trader.** Start the fork with `--skip-signature-verification` and add a pubkey-only "surfnet signer" so Vulcan can act as any registered mainnet trader (a whale, a liquidation candidate) without holding its key. This is the fastest path to "trade on historical mainnet state": snapshot a trader plus the markets at time T, replay decisions against it.
2. **On-chain trader-state reader.** Decode the trader account with `phoenix_rise::accounts` so `portfolio` / `position` work in `--surfnet` mode from RPC instead of the API. Gate behind `ctx.surfnet`.
3. **Register on the fork.** Clone a registered trader account and patch its authority bytes to a local wallet (`[[account]]` override), or replicate the onboarder permission on the fork so `RegisterTrader + OnboardTraderDelegated` can be signed locally.
4. **Scenario library.** Curated snapshots (`scenarios/*.toml` + `snapshots/*.json`) capturing notable market states, exported with `surfnet snapshot`. Historical *account* state cannot be fetched from a standard RPC, so "historical" means snapshots taken at the time or reconstructed via `[[account]]` overrides.
5. **Oracle streaming.** Use `surfnet_streamAccount` to keep oracle accounts live from mainnet while the fork's clock is paused, or patch them from `[[account]]` to simulate crashes and funding spikes.
6. **Strategy runners in surfnet mode.** Add `--mode surfnet` alongside `paper` / `dry_run` / `confirm_each` / `auto_execute` so TWAP, grid, and TA runners execute real transactions on the fork.
7. **MCP tools.** Expose `vulcan_surfnet_*` tools in the registry (paper-safe: they never touch mainnet) and a `vulcan://surfnet` resource.
8. **txtx runbooks.** Ship a `txtx.yml` with runbooks for common setups (seed markets, deploy a patched program) and let `[[runbook]]` reference them by ID.

## Design notes

- The Surfnet is spawned with `process_group(0)`, so it survives the CLI exiting and terminal hangups; `surfnet stop` sends SIGTERM to the recorded pid.
- State lives in `~/.vulcan/surfnet.json`; `surfnet.log` captures Surfpool output; `~/.vulcan/surfnet/` is the working directory (manifest, logs).
- Cheatcode calls use a small JSON-RPC client over the existing `reqwest` client. Surfpool expects `absoluteTimestamp` in **milliseconds**; Vulcan takes seconds everywhere and converts.
- Startup airdrops are opt-in (`--airdrop-amount 0` by default) so Surfpool does not silently fund `~/.config/solana/id.json`.
- Surfnet commands are never "dangerous": they cannot touch mainnet. Trading commands run with `--surfnet` keep their normal `--yes` / acknowledgement gates.
