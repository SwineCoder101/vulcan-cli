//! Paymaster support: a stored wallet that pays transaction fees (and
//! registration rent) instead of the trader wallet.
//!
//! On Solana the fee payer is simply the first signer of a transaction, so a
//! paymaster is a second local signer. It gains no authority over the trader's
//! funds or positions; Phoenix still requires the trader wallet to sign.

use crate::context::AppContext;
use crate::error::VulcanError;
use crate::wallet::{ResolvedSigner, WalletFile};
use solana_pubkey::Pubkey;
use std::str::FromStr;

/// A stored sponsor wallet validated for use as fee payer, before any network access.
///
/// Loading is offline (wallet-store lookup and pubkey checks) so a bad name fails
/// fast and dry runs can report the sponsor. The signer is only unlocked when a
/// transaction is actually submitted. A sponsor that is the trader wallet itself
/// resolves to `None`: the trader simply pays its own fees.
pub struct FeePayerWallet {
    wallet_file: WalletFile,
    pub pubkey: Pubkey,
}

impl FeePayerWallet {
    pub fn load(
        ctx: &AppContext,
        name: &str,
        trader_authority: Pubkey,
    ) -> Result<Option<Self>, VulcanError> {
        let name = name.trim();
        if name.is_empty() || !ctx.wallet_store.exists(name) {
            return Err(VulcanError::auth(
                "FEE_PAYER_WALLET_NOT_FOUND",
                format!(
                    "Fee payer wallet '{name}' not found. Use `vulcan wallet list` for stored names."
                ),
            ));
        }
        let wallet_file = ctx
            .wallet_store
            .load(name)
            .map_err(|e| VulcanError::auth("FEE_PAYER_WALLET_NOT_FOUND", e.to_string()))?;
        let pubkey = Pubkey::from_str(&wallet_file.public_key)
            .map_err(|e| VulcanError::validation("INVALID_PUBKEY", e.to_string()))?;
        if pubkey == trader_authority {
            // The trader is paying for itself; no second signer needed.
            return Ok(None);
        }
        Ok(Some(Self {
            wallet_file,
            pubkey,
        }))
    }

    /// Unlock the sponsor wallet for signing (prompts for a password if needed).
    pub async fn signer(&self) -> Result<ResolvedSigner, VulcanError> {
        let password = if self.wallet_file.is_local_encrypted() {
            Some(crate::commands::trade::prompt_password()?)
        } else {
            None
        };
        let signer =
            ResolvedSigner::from_wallet_file(&self.wallet_file, password.as_deref()).await?;
        if signer.authority != self.pubkey {
            return Err(VulcanError::auth(
                "SIGNER_PUBKEY_MISMATCH",
                format!(
                    "Fee payer signer pubkey {} does not match wallet authority {}",
                    signer.authority, self.pubkey
                ),
            ));
        }
        Ok(signer)
    }
}

/// Resolve the fee payer for a transaction signed by `trader_authority`.
///
/// Precedence: explicit per-call name > global `--fee-payer` / linked paymaster
/// (`ctx.fee_payer`) > none (the trader wallet pays its own fees). A sponsor equal
/// to the trader also yields `None`, so one linked paymaster can serve several
/// trader wallets and be selected as a trader itself without special-casing.
pub fn resolve_fee_payer(
    ctx: &AppContext,
    explicit: Option<&str>,
    trader_authority: Pubkey,
) -> Result<Option<FeePayerWallet>, VulcanError> {
    let name = explicit
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or(ctx.fee_payer.as_deref());
    match name {
        Some(n) => FeePayerWallet::load(ctx, n, trader_authority),
        None => Ok(None),
    }
}

pub(crate) const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Render lamports as a SOL string without trailing zero padding.
pub(crate) fn format_sol_lamports(lamports: u64) -> String {
    let whole = lamports / LAMPORTS_PER_SOL;
    let fractional = lamports % LAMPORTS_PER_SOL;
    let mut value = format!("{whole}.{fractional:09}");
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.push('0');
    }
    value
}

/// Fail with a clear error when the paymaster cannot cover a transaction fee.
pub(crate) fn check_fee_balance(
    payer: Pubkey,
    balance_lamports: u64,
    fee_lamports: u64,
) -> Result<(), VulcanError> {
    if balance_lamports < fee_lamports {
        return Err(VulcanError::validation(
            "INSUFFICIENT_SOL_FOR_FEE",
            format!(
                "Fee payer {payer} has {} SOL, but this transaction needs {} SOL in fees. Fund the paymaster wallet with SOL and retry.",
                format_sol_lamports(balance_lamports),
                format_sol_lamports(fee_lamports),
            ),
        ));
    }
    Ok(())
}

/// Look up the paymaster's SOL balance and the fee for `message`, then check it.
pub(crate) fn ensure_sol_for_fee(
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    payer: Pubkey,
    message: &solana_sdk::message::Message,
) -> Result<(), VulcanError> {
    let fee_lamports = rpc
        .get_fee_for_message(message)
        .map_err(|e| VulcanError::network("FEE_ESTIMATE_FAILED", e.to_string()))?;
    let balance_lamports = rpc
        .get_balance(&payer)
        .map_err(|e| VulcanError::network("RPC_BALANCE_FAILED", e.to_string()))?;
    check_fee_balance(payer, balance_lamports, fee_lamports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_check_rejects_underfunded_paymaster_with_amounts_in_message() {
        let payer = Pubkey::new_unique();
        let err = check_fee_balance(payer, 4_000, 5_000).expect_err("4000 < 5000 lamports");
        assert_eq!(err.code, "INSUFFICIENT_SOL_FOR_FEE");
        assert!(err.message.contains(&payer.to_string()));
        assert!(
            err.message.contains("0.000004 SOL"),
            "balance: {}",
            err.message
        );
        assert!(err.message.contains("0.000005 SOL"), "fee: {}", err.message);
    }

    #[test]
    fn fee_check_passes_when_balance_covers_fee() {
        let payer = Pubkey::new_unique();
        assert!(check_fee_balance(payer, 5_000, 5_000).is_ok());
        assert!(check_fee_balance(payer, 1_000_000, 5_000).is_ok());
    }
}
