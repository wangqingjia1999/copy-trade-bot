use crate::{
    blacklist, constants, metrics::SwapMetrics, process_swap::process_swap,
};
use carbon_core::instruction::{InstructionMetadata, NestedInstruction};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
use tracing::{debug, error};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Dex {
    RaydiumAmmV4,
    RaydiumClmm,
    RaydiumCpmm,
    RaydiumLaunchPad,
    MeteoraDlmm,
    MeteoraPools,
    MeteoraDammV2,
    Whirlpools,
    PumpSwap,
    Pumpfun,
    Unknown,
}

impl Dex {
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            constants::RAYDIUM_AMM_V4_PROGRAM_ID => Some(Dex::RaydiumAmmV4),
            constants::RAYDIUM_CLMM_PROGRAM_ID => Some(Dex::RaydiumClmm),
            constants::RAYDIUM_CPMM_PROGRAM_ID => Some(Dex::RaydiumCpmm),
            constants::RAYDIUM_LAUNCH_PAD_PROGRAM_ID => Some(Dex::RaydiumLaunchPad),
            constants::METEORA_DLMM_PROGRAM_ID => Some(Dex::MeteoraDlmm),
            constants::METEORA_POOLS_PROGRAM_ID => Some(Dex::MeteoraPools),
            constants::METEORA_DAMM_V2_PROGRAM_ID => Some(Dex::MeteoraDammV2),
            constants::WHIRLPOOLS_PROGRAM_ID => Some(Dex::Whirlpools),
            constants::PUMP_SWAP_PROGRAM_ID => Some(Dex::PumpSwap),
            constants::PUMPFUN_PROGRAM_ID => Some(Dex::Pumpfun),
            _ => Some(Dex::Unknown),
        }
    }
}

impl ToString for Dex {
    fn to_string(&self) -> String {
        match self {
            Dex::RaydiumAmmV4 => constants::RAYDIUM_AMM_V4_PROGRAM_ID.to_string(),
            Dex::RaydiumClmm => constants::RAYDIUM_CLMM_PROGRAM_ID.to_string(),
            Dex::RaydiumCpmm => constants::RAYDIUM_CPMM_PROGRAM_ID.to_string(),
            Dex::RaydiumLaunchPad => constants::RAYDIUM_LAUNCH_PAD_PROGRAM_ID.to_string(),
            Dex::MeteoraDlmm => constants::METEORA_DLMM_PROGRAM_ID.to_string(),
            Dex::MeteoraPools => constants::METEORA_POOLS_PROGRAM_ID.to_string(),
            Dex::MeteoraDammV2 => constants::METEORA_DAMM_V2_PROGRAM_ID.to_string(),
            Dex::Whirlpools => constants::WHIRLPOOLS_PROGRAM_ID.to_string(),
            Dex::PumpSwap => constants::PUMP_SWAP_PROGRAM_ID.to_string(),
            Dex::Pumpfun => constants::PUMPFUN_PROGRAM_ID.to_string(),
            Dex::Unknown => "Unknown".to_string(),
        }
    }
}

pub struct TokenSwapHandler {
    pub metrics: Arc<SwapMetrics>,
}

impl TokenSwapHandler {
    pub fn new(metrics: Arc<SwapMetrics>) -> Self {
        Self { metrics }
    }

    pub fn spawn_swap_processor(
        &self,
        vaults: &HashSet<String>,
        fee_adas: Option<&HashSet<String>>,
        meta: &InstructionMetadata,
        nested_instructions: &[NestedInstruction],
        dex: Dex,
    ) {
        let metrics = self.metrics.clone();

        let vaults = vaults.clone();
        let fee_adas = fee_adas.cloned();
        let meta = meta.clone();
        let nested_instructions = nested_instructions.to_vec();
        let txn_id = meta.transaction_metadata.signature.to_string();

        metrics.increment_total_swaps();
        metrics.increment_pending_swaps();

        // Optional: filter multi-signer bundles
        // let signer_count = meta.transaction_metadata.message.header().num_required_signatures;
        // if signer_count > 1 {
        //     metrics.increment_skipped_multi_signer();
        //     return;
        // }

        // Skip blacklisted traders
        if blacklist::is_trader_in_blacklist(&meta.transaction_metadata.fee_payer.to_string()) {
            metrics.increment_skipped_blacklist();
            return;
        }

        // Parse bloom copy-trading logs
        let logs = &meta.transaction_metadata.meta.log_messages;

        // Detect Bloom Router logs
        let has_bloom_router = logs
            .as_ref()
            .map(|logs| logs.iter().any(|log| log.contains(" Bloom Router ")))
            .unwrap_or(false);
        
        if has_bloom_router {
            debug!("Found Bloom Router transaction: {}", txn_id);
        }

        tokio::spawn(async move {
            match process_swap(
                &vaults,
                fee_adas.as_ref(),
                &meta,
                &nested_instructions,
                &metrics,
                dex.clone(),
            )
            .await
            {
                Ok(_) => {
                }
                Err(e) => {
                    metrics.increment_failed_swaps();
                    error!(
                        ?e,
                        "Transaction: https://solscan.io/tx/{}",
                        meta.transaction_metadata.signature
                    );
                }
            }
        });
    }
}
