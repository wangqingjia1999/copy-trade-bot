use crate::constants::WSOL_MINT_KEY_STR;
use crate::diffs::{
    extra_mint_details_from_tx_metadata, process_token_transfers, DiffsError,
    DiffsResult, TokenTransferDetails, SPL_TOKEN_TRANSFER_PROCESSOR,
};
use crate::handler::token_swap_handler::Dex;
use crate::metrics::SwapMetrics;
use crate::sol_price::get_sol_price;
use crate::util::{insert_trade_background, sol_to_decimals};
use std::sync::Arc;
use crate::{
    price::Trade,
};
use anyhow::{Context, Result};
use carbon_core::instruction::{InstructionMetadata, NestedInstruction};
use chrono::Utc;
use std::collections::HashSet;
use tracing::{debug, info, warn};

static DEBUG: once_cell::sync::Lazy<bool> =
    once_cell::sync::Lazy::new(|| std::env::var("DEBUG").is_ok());

/// Validates whether a token transfer involves a known vault account.
///
/// This function checks if either the source or destination address of a token transfer
/// matches any address in the provided set of vault addresses. It's used to filter token
/// transfers that are relevant to DEX or AMM operations by ensuring they interact with
/// a liquidity pool vault.
/// For a real-world example, see:
/// https://solscan.io/tx/2usSAGxq35GJxQxVKHQ7NHBDnJim95Jyk3AeFrRAcpHc2TJUH3bjhVSvtAWcxnqnQyJFzpPFgJvMHNkTuQ8t779f
pub fn is_valid_vault_transfer(
    transfer: &TokenTransferDetails,
    vaults: &HashSet<String>,
    fee_adas: Option<&HashSet<String>>,
) -> bool {
    // Early return if it's a fee transfer
    if let Some(fee_adas) = fee_adas {
        if fee_adas.contains(&transfer.destination) {
            return false;
        }
    }
    vaults.contains(&transfer.destination) || vaults.contains(&transfer.source)
}

pub async fn process_swap(
    vaults: &HashSet<String>,
    fee_adas: Option<&HashSet<String>>,
    meta: &InstructionMetadata,
    nested_instructions: &[NestedInstruction],
    metrics: &SwapMetrics,
    dex: Dex,
) -> Result<()> {
    // Decrement pending swaps when this function exits
    let _pending_guard = PendingSwapGuard(metrics);
    let tx_meta = meta.transaction_metadata.clone();

    let mint_details = extra_mint_details_from_tx_metadata(&tx_meta);

    let inner_transfers = SPL_TOKEN_TRANSFER_PROCESSOR
        .decode_token_transfer_with_vaults_from_nested_instructions(
            nested_instructions,
            &mint_details,
            dex.clone(),
        );

    let transfers = inner_transfers
        .into_iter()
        .filter(|d| is_valid_vault_transfer(d, vaults, fee_adas))
        .collect::<Vec<_>>();

    if transfers.iter().all(|d| d.ui_amount < 0.1) {
        debug!("skipping tiny diffs");
        metrics.increment_skipped_tiny_swaps();
        return Ok(());
    }

    if transfers.len() > 3 || transfers.len() < 2 {
        debug!(
            "https://solscan.io/tx/{} skipping swap with unexpected number of tokens: {}",
            tx_meta.signature, transfers.len()
        );
        metrics.increment_skipped_unexpected_number_of_tokens();
        return Ok(());
    }

    process_two_token_swap(
        vaults,
        &transfers,
        &meta,
        metrics,
        dex,
    )
    .await
    .context("failed to process two token swap")
}

// Helper function to process a single two-token swap
#[allow(clippy::too_many_arguments)]
pub async fn process_two_token_swap(
    vaults: &HashSet<String>,
    transfers: &[TokenTransferDetails],
    meta: &InstructionMetadata,
    metrics: &SwapMetrics,
    dex: Dex,
) -> Result<()> {
    let DiffsResult {
        price,
        swap_amount,
        token_mint,
        token_amount,
        sol_amount,
        is_buy,
    } = match process_token_transfers(vaults, transfers, get_sol_price()) {
        Ok(result) => result,
        Err(e) => {
            match e {
                DiffsError::NonWsolsSwap => {
                    metrics.increment_skipped_non_wsol();
                }
                DiffsError::ExpectedExactlyTwoTokenBalanceDiffs => {
                    metrics.increment_skipped_unexpected_number_of_tokens();
                }
            }
            return Ok(());
        }
    };

    let transaction_metadata = meta.transaction_metadata.clone();

    let token_min_str = token_mint.to_string();
    let owner = transaction_metadata.fee_payer.to_string();
    let signature = transaction_metadata.signature.to_string();
    let instruction_idx = meta.index;

    let (
        token_bought_mint_address,
        token_bought_amount,
        token_sold_mint_address,
        token_sold_amount,
    ) = if is_buy {
        (
            token_min_str.clone(),
            token_amount,
            WSOL_MINT_KEY_STR.to_string(),
            sol_amount,
        )
    } else {
        (
            WSOL_MINT_KEY_STR.to_string(),
            sol_amount,
            token_min_str,
            token_amount,
        )
    };

    let trade = Trade {
        trader_id: owner,
        tx_id: signature,
        token_bought_mint_address,
        token_sold_mint_address,
        token_bought_amount: token_bought_amount,
        token_sold_amount: token_sold_amount,
        amount_usd: swap_amount,
        block_date: Utc::now().timestamp(),
        block_slot: transaction_metadata.slot,
        instruction_idx: instruction_idx as i32,
        fee: sol_to_decimals(transaction_metadata.meta.fee),
        dex: dex.to_string(),
        robot: "".to_string(),
    };

    insert_trade_background(Arc::new(metrics.clone()), &meta, &trade);

    metrics.set_latest_update_slot(transaction_metadata.slot);
    
    Ok(())
}

// Helper struct to decrement pending swaps when dropped
struct PendingSwapGuard<'a>(&'a SwapMetrics);

impl Drop for PendingSwapGuard<'_> {
    fn drop(&mut self) {
        self.0.decrement_pending_swaps();
    }
}
