use clickhouse::Row;
use serde::{Deserialize, Serialize};
use crate::constants::WSOL_MINT_KEY;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    pub coin_price: f64,
    pub coin_mint: String,
    pub pc_mint: String,
    pub coin_decimals: u64,
    pub pc_decimals: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Row)]
pub struct PriceUpdate {
    pub name: String,
    pub pubkey: String,
    pub price: f64,
    pub market_cap: f64,
    pub timestamp: u64,
    pub slot: u64,
    pub swap_amount: f64, // denoted as usd
    pub owner: String,
    pub signature: String,
    pub multi_hop: bool,
    pub is_buy: bool,
    pub is_pump: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, clickhouse::Row)]
pub struct Trade {
    pub token_bought_amount: f64,
    pub token_bought_mint_address: String,
    pub token_sold_amount: f64,
    pub token_sold_mint_address: String,
    pub block_date: i64, // Avoid Datetime for compatibility
    pub trader_id: String,
    pub tx_id: String,
    pub instruction_idx: i32,
    pub amount_usd: f64,
    pub block_slot: u64,
    pub fee: f64,
    pub dex: String,
    pub robot: String,
}

impl Trade {
    /// Selling SOL means buying the token
    pub fn is_buy_token(&self) -> bool {
        self.token_sold_mint_address == WSOL_MINT_KEY.to_string()
    }

    /// Calculate token price in SOL
    pub fn calculate_token_sol_price(&self) -> f64 {
        // Buy token: sold is wSOL
        if self.is_buy_token() {
            self.token_sold_amount / self.token_bought_amount
        } else { // Sell token: bought is wSOL
            self.token_bought_amount / self.token_sold_amount
        }
    }

    /// Get token amount for buy/sell
    pub fn get_token_amount(&self) -> f64 {
        if self.is_buy_token() {
            self.token_bought_amount
        } else {
            self.token_sold_amount
        }
    }

    /// Get SOL amount for buy/sell
    pub fn get_sol_amount(&self) -> f64 {
        if self.is_buy_token() {
            self.token_sold_amount
        } else {
            self.token_bought_amount
        }
    }

    /// Get token mint for buy/sell
    pub fn get_token_mint(&self) -> &str {
        if self.is_buy_token() {
            &self.token_bought_mint_address
        } else {
            &self.token_sold_mint_address
        }
    }
}
