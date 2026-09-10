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
/// transaction is actually submitted.
pub struct FeePayerWallet {
    wallet_file: WalletFile,
    pub pubkey: Pubkey,
}

impl FeePayerWallet {
    pub fn load(
        ctx: &AppContext,
        name: &str,
        trader_authority: Pubkey,
    ) -> Result<Self, VulcanError> {
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
            return Err(VulcanError::validation(
                "FEE_PAYER_IS_TRADER",
                "Fee payer wallet is the same as the trader wallet; omit --fee-payer or run `vulcan wallet clear-fee-payer`.",
            ));
        }
        Ok(Self {
            wallet_file,
            pubkey,
        })
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
/// (`ctx.fee_payer`) > none (the trader wallet pays its own fees).
pub fn resolve_fee_payer(
    ctx: &AppContext,
    explicit: Option<&str>,
    trader_authority: Pubkey,
) -> Result<Option<FeePayerWallet>, VulcanError> {
    let name = explicit
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or(ctx.fee_payer.as_deref());
    name.map(|n| FeePayerWallet::load(ctx, n, trader_authority))
        .transpose()
}
