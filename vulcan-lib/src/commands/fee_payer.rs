//! Paymaster: a stored wallet that pays transaction fees and registration rent
//! instead of the trader wallet. It co-signs as fee payer; the trader still signs.

use crate::commands::trade::prompt_password;
use crate::context::AppContext;
use crate::error::VulcanError;
use crate::wallet::{ResolvedSigner, WalletFile, WalletStore};
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::message::Message;
use std::str::FromStr;

/// Sponsor wallet validated offline; unlocked only when a transaction is signed.
pub struct FeePayerWallet {
    wallet_file: WalletFile,
    pub pubkey: Pubkey,
}

impl FeePayerWallet {
    /// Returns `None` when the sponsor is the trader itself.
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
            return Ok(None);
        }
        Ok(Some(Self {
            wallet_file,
            pubkey,
        }))
    }

    pub async fn signer(&self) -> Result<ResolvedSigner, VulcanError> {
        let password = if self.wallet_file.is_local_encrypted() {
            Some(prompt_password()?)
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

/// Precedence: explicit name, then `--fee-payer`, then the linked paymaster.
pub fn resolve_fee_payer(
    ctx: &AppContext,
    explicit: Option<&str>,
    trader_authority: Pubkey,
) -> Result<Option<FeePayerWallet>, VulcanError> {
    match resolve_fee_payer_name(&ctx.wallet_store, ctx.fee_payer.as_deref(), explicit)? {
        Some(name) => FeePayerWallet::load(ctx, &name, trader_authority),
        None => Ok(None),
    }
}

pub fn resolve_fee_payer_name(
    store: &WalletStore,
    flag_override: Option<&str>,
    explicit: Option<&str>,
) -> Result<Option<String>, VulcanError> {
    let clean = |s: Option<&str>| {
        s.map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    };
    if let Some(name) = clean(explicit).or_else(|| clean(flag_override)) {
        return Ok(Some(name));
    }
    store
        .fee_payer()
        .map_err(|e| VulcanError::io("FEE_PAYER_LINK_READ_FAILED", e.to_string()))
}

pub(crate) const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

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

pub(crate) fn ensure_sol_for_fee(
    rpc: &RpcClient,
    payer: Pubkey,
    message: &Message,
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
    use crate::wallet::WalletSignerConfig;
    use std::mem::ManuallyDrop;

    fn store_with(names: &[&str]) -> (tempfile::TempDir, WalletStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = WalletStore::new(dir.path()).unwrap();
        for name in names {
            let file = WalletFile::remote(
                name.to_string(),
                Pubkey::new_unique().to_string(),
                WalletSignerConfig::Vault {
                    vault_addr: "https://vault.example".into(),
                    token_env: "VAULT_TOKEN".into(),
                    key_name: name.to_string(),
                },
                "now".into(),
            );
            store.save(&file).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn resolver_prefers_explicit_then_flag_then_linked_store() {
        let (_dir, store) = store_with(&["linked", "flagged", "explicit"]);
        assert_eq!(resolve_fee_payer_name(&store, None, None).unwrap(), None);

        store.set_fee_payer("linked").unwrap();
        assert_eq!(
            resolve_fee_payer_name(&store, None, None)
                .unwrap()
                .as_deref(),
            Some("linked"),
            "linked paymaster applies with no flags"
        );
        assert_eq!(
            resolve_fee_payer_name(&store, Some("flagged"), None)
                .unwrap()
                .as_deref(),
            Some("flagged"),
            "--fee-payer overrides the link"
        );
        assert_eq!(
            resolve_fee_payer_name(&store, Some("flagged"), Some(" explicit "))
                .unwrap()
                .as_deref(),
            Some("explicit"),
            "a per-call name wins and is trimmed"
        );
        assert_eq!(
            resolve_fee_payer_name(&store, Some("  "), None)
                .unwrap()
                .as_deref(),
            Some("linked"),
            "a blank flag is ignored"
        );

        store.clear_fee_payer().unwrap();
        assert_eq!(resolve_fee_payer_name(&store, None, None).unwrap(), None);
    }

    #[test]
    fn resolver_reads_the_link_live_between_calls() {
        let (_dir, store) = store_with(&["sponsor"]);
        assert_eq!(resolve_fee_payer_name(&store, None, None).unwrap(), None);
        store.set_fee_payer("sponsor").unwrap();
        assert_eq!(
            resolve_fee_payer_name(&store, None, None)
                .unwrap()
                .as_deref(),
            Some("sponsor")
        );
    }

    #[test]
    fn linking_an_unknown_wallet_is_rejected_and_dangling_links_are_ignored() {
        let (dir, store) = store_with(&["sponsor"]);
        let dir = ManuallyDrop::new(dir);
        assert!(store.set_fee_payer("nope").is_err());
        store.set_fee_payer("sponsor").unwrap();
        std::fs::remove_file(store.wallet_path("sponsor")).unwrap();
        let _keep = &dir;
        assert_eq!(
            resolve_fee_payer_name(&store, None, None).unwrap(),
            None,
            "a link to a missing wallet must not become a broken fee payer"
        );
    }

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
