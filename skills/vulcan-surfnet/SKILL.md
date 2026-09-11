---
name: vulcan-surfnet
version: 0.1.0
description: "Local Surfpool mainnet fork for scenario testing: start/stop a Surfnet, fund wallets with fake SOL/USDC, time-travel, snapshot, and apply scenario files. Use when the user wants to test Vulcan's real on-chain flows or replay a market state without risking funds."
metadata:
  openclaw:
    category: "finance"
  requires:
    bins: ["vulcan", "surfpool"]
    skills: ["vulcan", "vulcan-execution-modes"]
---

# vulcan-surfnet

A **Surfnet** is a local [Surfpool](https://surfpool.run) fork of Solana mainnet. Vulcan can start one, fund wallets on it, move its clock, snapshot it, and point every other command at it with `--surfnet`. Nothing on a Surfnet touches mainnet or real funds.

Use it when paper trading is not enough: the user wants real transactions, real Phoenix programs, or a reproducible market state. Read `docs/surfnet-integration.md` for the current limits (Phoenix API reads still reflect mainnet).

## Lifecycle

```bash
vulcan surfnet start                          # mainnet fork on :8899, logs in ~/.vulcan/surfnet.log
vulcan surfnet status -o json                 # running, slot, clock
vulcan surfnet fund trader --sol 5 --usdc 10000
vulcan --surfnet wallet balance               # any command, targeted at the fork
vulcan surfnet time-travel --forward 1h
vulcan surfnet clock pause
vulcan surfnet snapshot --out snapshots/state.json
vulcan surfnet stop
```

`surfpool` must be installed (`curl -sL https://run.surfpool.run/ | bash`). `SURFPOOL_NOT_INSTALLED` tells the user how.

## Scenarios

Reproducible setups live in TOML files. Print a template with `vulcan surfnet scenario example`, check one with `scenario validate <file>`, apply it with `scenario run <file>` (add `--start` to also start the Surfnet from its `[network]` section). Steps run in order: account overrides, funding, txtx runbooks, clock.

## Rules

- Surfnet commands are safe and need no acknowledgement. Trading commands run with `--surfnet` still require `--yes` / `acknowledged: true` — keep the user's habits intact.
- Never pass `--surfnet` together with `--rpc-url`; they conflict.
- `portfolio`, `position`, and `history` read from the Phoenix API and show **mainnet** state even with `--surfnet`. Verify fork state with `--surfnet wallet balance` or RPC reads until the on-chain reader lands.
- A scenario with `skip_signature_verification = true` lets any pubkey sign on the fork. Only use it for impersonation tests the user asked for, and say so in the summary.
