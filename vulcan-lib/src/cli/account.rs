//! Account subcommand definitions.

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Register a trader account on Phoenix
    Register {
        /// Referral code (optional; the default code is used when omitted)
        #[arg(long)]
        referral_code: Option<String>,

        /// Stored wallet name that pays the transaction fee and trader-account rent
        /// instead of the trader wallet (sponsored registration). Both wallets sign.
        #[arg(long)]
        fee_payer: Option<String>,
    },

    /// Show trader account details (PDA, subaccounts, margin mode)
    Info,

    /// List all subaccounts
    Subaccounts,

    /// Create a new subaccount
    CreateSubaccount {
        /// PDA index
        #[arg(long, default_value = "0")]
        pda_index: u8,

        /// Subaccount index
        #[arg(long)]
        subaccount_index: u8,
    },
}
