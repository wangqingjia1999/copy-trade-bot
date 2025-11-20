use crate::{
    constants::{
        TOKEN_2022_PROGRAM_ID,
        TOKEN_PROGRAM_ID, WSOL_MINT_KEY_STR,
    },
    handler::token_swap_handler::Dex,
};
use anyhow::Result;
use carbon_core::{
    deserialize::ArrangeAccounts,
    instruction::{DecodedInstruction, InstructionDecoder, NestedInstruction},
    transaction::TransactionMetadata,
};
use carbon_token_2022_decoder::{
    instructions::transfer::{
        Transfer as Token2022Transfer, TransferInstructionAccounts,
    },
    instructions::transfer_checked::{
        TransferChecked as Token2022TransferChecked,
        TransferCheckedInstructionAccounts,
    },
    instructions::Token2022Instruction,
    Token2022Decoder,
};
use carbon_token_program_decoder::{
    instructions::transfer::{Transfer, TransferAccounts},
    instructions::transfer_checked::{
        TransferChecked, TransferCheckedAccounts,
    },
    instructions::TokenProgramInstruction,
    TokenProgramDecoder,
};
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    TransactionTokenBalance, UiTransactionTokenBalance,
};
use spl_token::amount_to_ui_amount;
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};
use tracing::{error};

pub trait TokenBalanceInfo {
    fn get_mint(&self) -> &str;
    fn get_ui_amount(&self) -> Option<f64>;
    fn get_owner(&self) -> &str;
    fn get_account_index(&self) -> usize;
    fn get_address(&self) -> &str;
}

impl TokenBalanceInfo for TransactionTokenBalance {
    fn get_mint(&self) -> &str {
        &self.mint
    }

    fn get_ui_amount(&self) -> Option<f64> {
        self.ui_token_amount.ui_amount
    }

    fn get_owner(&self) -> &str {
        &self.owner
    }

    fn get_account_index(&self) -> usize {
        self.account_index as usize
    }

    fn get_address(&self) -> &str {
        ""
    }
}

impl TokenBalanceInfo for UiTransactionTokenBalance {
    fn get_mint(&self) -> &str {
        &self.mint
    }

    fn get_ui_amount(&self) -> Option<f64> {
        self.ui_token_amount.ui_amount
    }

    fn get_owner(&self) -> &str {
        self.owner.as_ref().map(|s| s.as_str()).unwrap_or_default()
    }

    fn get_account_index(&self) -> usize {
        self.account_index as usize
    }

    fn get_address(&self) -> &str {
        ""
    }
}

#[derive(Debug)]
pub struct DiffsResult {
    pub price: f64,
    pub swap_amount: f64,
    pub token_mint: String,
    pub token_amount: f64,
    pub sol_amount: f64,
    pub is_buy: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DiffsError {
    #[error("Expected exactly 2 token balance diffs")]
    ExpectedExactlyTwoTokenBalanceDiffs,
    #[error("Non-WSOL swap")]
    NonWsolsSwap,
}

pub fn process_token_transfers(
    vaults: &HashSet<String>,
    transfers: &[TokenTransferDetails],
    sol_price: f64,
) -> Result<DiffsResult, DiffsError> {
    if transfers.len() != 2 {
        return Err(DiffsError::ExpectedExactlyTwoTokenBalanceDiffs);
    }

    let (token_0_transfer, token_1_transfer) = (&transfers[0], &transfers[1]);

    let (sol_transfer, token_transfer) = match (
        token_0_transfer.mint.as_str(),
        token_1_transfer.mint.as_str(),
    ) {
        (WSOL_MINT_KEY_STR, _) => (token_0_transfer, token_1_transfer),
        (_, WSOL_MINT_KEY_STR) => (token_1_transfer, token_0_transfer),
        _ => return Err(DiffsError::NonWsolsSwap),
    };

    // is_buy: whether buying token (sol into vault or token out of vault)
    let is_buy = vaults.contains(&sol_transfer.destination)
        || vaults.contains(&token_transfer.source);

    let token_mint = token_transfer.mint.clone();
    let token_amount = token_transfer.ui_amount;
    let sol_amount = sol_transfer.ui_amount;

    let price = (sol_amount / token_amount) * sol_price;
    let swap_amount = sol_amount * sol_price;

    Ok(DiffsResult {
        price,
        swap_amount,
        token_mint: token_mint.to_string(),
        is_buy,
        token_amount,
        sol_amount,
    })
}

#[derive(Debug, Clone)]
pub struct Diff {
    pub mint: String,
    pub pre_amount: f64,
    pub post_amount: f64,
    pub diff: f64,
    pub owner: String,
    pub address: String,
}

pub fn get_transfers(
    sell_src: String,
    sell_dest: String,
    sell_authority: String,
    buy_src: String,
    buy_dest: String,
    buy_authority: String,
    mint_detail: &HashMap<String, MintDetail>,
) -> Vec<TokenTransferDetails> {
    let (buy_mint_detail, sell_mint_detail) = match (
        mint_detail
            .iter()
            .find(|(address, _)| **address == buy_src),
        mint_detail
            .iter()
            .find(|(address, _)| **address == sell_dest),
    ) {
        (Some(buy), Some(sell)) => (buy.1, sell.1),
        _ => return vec![],
    };

    let buy_transfer = TokenTransferDetails {
        program_id: TOKEN_2022_PROGRAM_ID.to_string(),
        source: buy_src,
        destination: buy_dest,
        authority: buy_authority,
        mint: buy_mint_detail.mint.clone(),
        decimals: buy_mint_detail.decimals,
        amount: buy_mint_detail.diff as u64,
        ui_amount: buy_mint_detail.diff,
    };

    let sell_transfer = TokenTransferDetails {
        program_id: TOKEN_2022_PROGRAM_ID.to_string(),
        source: sell_src,
        destination: sell_dest,
        authority: sell_authority,
        mint: sell_mint_detail.mint.clone(),
        decimals: sell_mint_detail.decimals,
        amount: sell_mint_detail.diff as u64,
        ui_amount: sell_mint_detail.diff,
    };

    vec![sell_transfer, buy_transfer]
}

/// Represents the details of a token transfer instruction
///
/// This struct contains all the relevant information about a token transfer,
/// including the source and destination accounts, token mint, authority,
/// amount and decimal precision.
///
/// # Fields
/// * `program_id` - The ID of the token program executing the transfer (Token or Token-2022)
/// * `source` - The source account address the tokens are being transferred from
/// * `destination` - The destination account address the tokens are being transferred to
/// * `mint` - Optional mint address
/// * `authority` - The account authorized of source account
/// * `amount` - The raw token amount being transferred (not adjusted for decimals)
/// * `decimals` - Optional decimal precision of the token
/// * `ui_amount` - The token amount in UI format (adjusted for decimals)
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, PartialEq)]
pub struct TokenTransferDetails {
    pub program_id: String,
    pub source: String,
    pub destination: String,
    pub mint: String,
    pub authority: String,
    pub decimals: u8,
    pub amount: u64,
    pub ui_amount: f64,
}

/// Implement the From trait for TokenTransferDetails
///
/// This macro implements the From trait for TokenTransferDetails, allowing
/// conversion from various account types to TokenTransferDetails.
///
/// # Parameters
/// * `$account_type`: The type of the token account to convert from
/// * `$program_id`: The program ID of the token program (Token or Token-2022)
///
/// # Returns
/// * A TokenTransferDetails struct with basic fields populated from the account
macro_rules! impl_into_token_transfer_details_with_mint {
    ($account_type:ty, $program_id:expr) => {
        impl From<$account_type> for TokenTransferDetails {
            fn from(accounts: $account_type) -> Self {
                Self {
                    program_id: $program_id.to_string(),
                    source: accounts.source.to_string(),
                    destination: accounts.destination.to_string(),
                    authority: accounts.authority.to_string(),
                    mint: accounts.mint.to_string(),
                    decimals: 0,
                    amount: 0,
                    ui_amount: 0.0,
                }
            }
        }
    };
}

macro_rules! impl_into_token_transfer_details_without_mint {
    ($account_type:ty, $program_id:expr) => {
        impl From<$account_type> for TokenTransferDetails {
            fn from(accounts: $account_type) -> Self {
                Self {
                    program_id: $program_id.to_string(),
                    source: accounts.source.to_string(),
                    destination: accounts.destination.to_string(),
                    authority: accounts.authority.to_string(),
                    mint: String::new(),
                    decimals: 0,
                    amount: 0,
                    ui_amount: 0.0,
                }
            }
        }
    };
}

impl_into_token_transfer_details_without_mint!(
    TransferAccounts,
    TOKEN_PROGRAM_ID
);
impl_into_token_transfer_details_with_mint!(
    TransferCheckedAccounts,
    TOKEN_PROGRAM_ID
);
impl_into_token_transfer_details_without_mint!(
    TransferInstructionAccounts,
    TOKEN_2022_PROGRAM_ID
);
impl_into_token_transfer_details_with_mint!(
    TransferCheckedInstructionAccounts,
    TOKEN_2022_PROGRAM_ID
);

/// A static instance of TokenTransferProcessor for global access
pub static SPL_TOKEN_TRANSFER_PROCESSOR: LazyLock<TokenTransferProcessor> =
    LazyLock::new(TokenTransferProcessor::new);

pub struct TokenTransferProcessor {
    pub token_decoder: TokenProgramDecoder,
    pub token_2022_decoder: Token2022Decoder,
}

pub fn process_token_transfer(
    instruction: DecodedInstruction<TokenProgramInstruction>,
) -> Option<TokenTransferDetails> {
    if !instruction.program_id.to_string().eq(&TOKEN_PROGRAM_ID) {
        return None;
    }
    match &instruction.data {
        TokenProgramInstruction::Transfer(t) => {
            Transfer::arrange_accounts(&instruction.accounts).map(|accounts| {
                let mut details = TokenTransferDetails::from(accounts);
                details.amount = t.amount;
                details
            })
        }
        TokenProgramInstruction::TransferChecked(t) => {
            TransferChecked::arrange_accounts(&instruction.accounts).map(
                |accounts| {
                    let mut details = TokenTransferDetails::from(accounts);
                    details.amount = t.amount;
                    details.decimals = t.decimals;
                    details.ui_amount =
                        amount_to_ui_amount(t.amount, t.decimals);
                    details
                },
            )
        }
        _ => None,
    }
}

pub fn process_token_2022_transfer(
    instruction: DecodedInstruction<Token2022Instruction>,
) -> Option<TokenTransferDetails> {
    if !instruction.program_id.to_string().eq(&TOKEN_2022_PROGRAM_ID) {
        return None;
    }

    match &instruction.data {
        Token2022Instruction::Transfer(t) => {
            Token2022Transfer::arrange_accounts(&instruction.accounts).map(
                |accounts| {
                    let mut details = TokenTransferDetails::from(accounts);
                    details.amount = t.amount;
                    details
                },
            )
        }
        Token2022Instruction::TransferChecked(t) => {
            Token2022TransferChecked::arrange_accounts(&instruction.accounts)
                .map(|accounts| {
                    let mut details = TokenTransferDetails::from(accounts);
                    details.amount = t.amount;
                    details.decimals = t.decimals;
                    details.ui_amount =
                        amount_to_ui_amount(t.amount, t.decimals);
                    details
                })
        }
        _ => None,
    }
}

impl TokenTransferProcessor {
    pub fn new() -> Self {
        Self {
            token_decoder: TokenProgramDecoder,
            token_2022_decoder: Token2022Decoder,
        }
    }

    pub fn try_decode_token_transfer(
        &self,
        instruction: &solana_sdk::instruction::Instruction,
    ) -> Option<TokenTransferDetails> {
        if instruction.program_id.to_string() != TOKEN_PROGRAM_ID {
            return None;
        }
        self.token_decoder
            .decode_instruction(instruction)
            .and_then(process_token_transfer)
    }

    pub fn try_decode_token_2022_transfer(
        &self,
        instruction: &solana_sdk::instruction::Instruction,
    ) -> Option<TokenTransferDetails> {
        if instruction.program_id.to_string() != TOKEN_2022_PROGRAM_ID {
            return None;
        }

        self.token_2022_decoder
            .decode_instruction(instruction)
            .and_then(process_token_2022_transfer)
    }

    pub fn parse_token_transfer_with_metadata(
        &self,
        mint_details: &HashMap<String, MintDetail>,
        instruction: &solana_sdk::instruction::Instruction,
        dex: Dex,
    ) -> Option<TokenTransferDetails> {
        let details = match instruction.program_id.to_string().as_ref() {
            TOKEN_PROGRAM_ID => self.try_decode_token_transfer(instruction),
            TOKEN_2022_PROGRAM_ID => {
                self.try_decode_token_2022_transfer(instruction)
            }
            _ => None,
        };

        details.map(|mut details| {
            update_token_transfer_details(&mut details, mint_details);
            details
        })
    }

    pub fn decode_token_transfer_with_vaults_from_nested_instructions(
        &self,
        nested_instructions: &[NestedInstruction],
        mint_details: &HashMap<String, MintDetail>,
        dex: Dex,
    ) -> Vec<TokenTransferDetails> {
        nested_instructions
            .iter()
            .filter_map(|instruction| {
                self.parse_token_transfer_with_metadata(
                    mint_details,
                    &instruction.instruction,
                    dex.clone(),
                )
            })
            .collect()
    }
}

impl Default for TokenTransferProcessor {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MintDetail {
    pub mint: String,
    pub owner: String,
    pub decimals: u8,
    pub diff: f64,
    pub account_idx: usize,
    pub account: String,
}

impl From<&solana_transaction_status::TransactionTokenBalance> for MintDetail {
    fn from(
        balance: &solana_transaction_status::TransactionTokenBalance,
    ) -> Self {
        Self {
            mint: balance.mint.clone(),
            owner: balance.owner.clone(),
            decimals: balance.ui_token_amount.decimals,
            account_idx: balance.account_index as usize,
            diff: 0.0,
            account: "".to_string(),
        }
    }
}

pub fn update_token_accounts_from_meta<'a>(
    signature: &Signature,
    accounts: &[Pubkey],
    balances: &[solana_transaction_status::TransactionTokenBalance],
    mint_details: &'a mut HashMap<String, MintDetail>,
    balance_map: &'a mut HashMap<(String, String, String), f64>,
) -> &'a mut HashMap<String, MintDetail> {
    for balance in balances {
        if let Some(pubkey) = accounts.get(balance.account_index as usize) {
            mint_details.insert(pubkey.to_string(), MintDetail::from(balance));
            balance_map.insert(
                (
                    balance.mint.clone(),
                    balance.owner.clone(),
                    pubkey.to_string(),
                ),
                balance.ui_token_amount.ui_amount.unwrap_or(0.0),
            );
        } else {
            error!(
                "Invalid account_index {} for signature: {}",
                balance.account_index, signature
            );
        }
    }
    mint_details
}

pub fn update_token_transfer_details(
    details: &mut TokenTransferDetails,
    mint_details: &HashMap<String, MintDetail>,
) {
    if let Some(mint_detail) = mint_details.get(&details.source) {
        update_details_from_mint(details, mint_detail);
    } else if let Some(mint_detail) = mint_details.get(&details.destination) {
        update_details_from_mint(details, mint_detail);
    }
}

/// Helper function to update token details from mint information
fn update_details_from_mint(
    token_transfer_details: &mut TokenTransferDetails,
    mint_detail: &MintDetail,
) {
    token_transfer_details.mint = mint_detail.mint.clone();
    token_transfer_details.decimals = mint_detail.decimals;
    token_transfer_details.ui_amount = amount_to_ui_amount(
        token_transfer_details.amount,
        mint_detail.decimals,
    );
}

pub fn extra_mint_details_from_tx_metadata(
    transaction_metadata: &TransactionMetadata,
) -> HashMap<String, MintDetail> {
    let mut mint_details = HashMap::new();
    let account_keys =
        transaction_metadata.message.static_account_keys().to_vec();
    let loaded_addresses = transaction_metadata.meta.loaded_addresses.clone();
    let accounts_address = [
        account_keys,
        loaded_addresses.writable,
        loaded_addresses.readonly,
    ]
    .concat();

    let mut pre_balances_map = HashMap::new();
    let mut post_balances_map = HashMap::new();

    let meta = &transaction_metadata.meta;
    if let Some(pre_balances) = meta.pre_token_balances.as_ref() {
        update_token_accounts_from_meta(
            &transaction_metadata.signature,
            &accounts_address,
            pre_balances,
            &mut mint_details,
            &mut pre_balances_map,
        );
    }
    if let Some(post_balances) = meta.post_token_balances.as_ref() {
        update_token_accounts_from_meta(
            &transaction_metadata.signature,
            &accounts_address,
            post_balances,
            &mut mint_details,
            &mut post_balances_map,
        );
    }

    // Record diff
    for ((mint, owner, address), pre_amount) in pre_balances_map.iter() {
        if let Some(post_amount) = post_balances_map.get(&(
            mint.clone(),
            owner.clone(),
            address.clone(),
        )) {
            let diff = post_amount - pre_amount;
            if let Some(mint_detail_mut) = mint_details.get_mut(address) {
                mint_detail_mut.account = address.clone();
                mint_detail_mut.diff = diff.abs();
            }
        }
    }    
    
    mint_details
}
