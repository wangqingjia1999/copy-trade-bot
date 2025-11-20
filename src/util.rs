use anyhow::Result;
use bb8_redis::{bb8, RedisConnectionManager};
use carbon_core::{instruction::InstructionMetadata, metrics::Metrics};
use chrono::Utc;
use ordered_float::OrderedFloat;
use solana_client::nonblocking::rpc_client::RpcClient;
use tracing::{debug, error, info};
use std::{fs::File, io::BufWriter, sync::Arc};
use sqlx::{postgres::PgPoolOptions, Pool, Postgres, Row};
use once_cell::sync::OnceCell;
use sha2::{Sha256, Digest};

use crate::{blacklist, clickhouse::ClickhouseDb, constants::{self, SOL_DECIMALS, TOKEN_DECIMALS, WSOL_MINT_KEY}, copy_trading::CopyTradingEngine, handler::token_swap_handler::Dex, key_dispatcher::GLOBAL_DISPATCHER, message_queue::RedisMessageQueue, metrics::{self, SwapMetrics}, orderbook::{get_order_book, LimitOrder}, price::Trade};

static PG_POOL: OnceCell<Pool<Postgres>> = OnceCell::new();

pub fn is_local() -> bool {
    use std::fs;
    use std::io::Read;

    fs::File::open("/proc/version")
        .and_then(|mut file| {
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            Ok(content.to_lowercase().contains("microsoft-standard"))
        })
        .unwrap_or(false)
}

pub fn make_rpc_client() -> Result<RpcClient> {
    let rpc_client = RpcClient::new(must_get_env("RPC_URL"));
    Ok(rpc_client)
}

pub async fn make_message_queue() -> Result<Arc<RedisMessageQueue>> {
    match is_local() {
        true => {
            let message_queue =
                RedisMessageQueue::new("redis://localhost:6379").await?;
            Ok(Arc::new(message_queue))
        }
        false => {
            let message_queue =
                RedisMessageQueue::new(must_get_env("REDIS_URL").as_str())
                    .await?;
            Ok(Arc::new(message_queue))
        }
    }
}

pub fn is_bloom_robot(meta: &InstructionMetadata) -> bool {
    // Parse bloom copy-trading logs
    let logs = &meta.transaction_metadata.meta.log_messages;
    
    // Detect Bloom Router log
    let has_bloom_router = logs.as_ref()
        .map(|logs| logs.iter().rev().any(|log| log.contains(constants::BLOOM_ROUTER_PROGRAM_ID)))
        .unwrap_or(false);
    has_bloom_router
}

pub async fn make_pg_db() -> Result<Pool<Postgres>> {
    if let Some(pool) = PG_POOL.get() {
        return Ok(pool.clone());
    }

    let solana_db_conn_str = "postgres://postgres:qwer@localhost:5432/solana_db?sslmode=disable";
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(solana_db_conn_str)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS trades (
            id SERIAL,
            token_bought_amount       NUMERIC(30, 6),
            token_bought_mint_address TEXT,
            token_sold_amount         NUMERIC(30, 6),
            token_sold_mint_address   TEXT,
            block_date                TIMESTAMP,
            trader_id                 TEXT,
            tx_id                     TEXT,
            instruction_idx           INT,
            amount_usd                NUMERIC(30, 6),
            block_slot                BIGINT,
            fee                       NUMERIC(20,9),
            dex                       TEXT,
            robot                     TEXT,
            PRIMARY KEY (tx_id, instruction_idx)
        );
        "#
    )
    .execute(&pool)
    .await?;

    PG_POOL.set(pool.clone()).expect("Failed to set PG_POOL");
    Ok(pool)
}

// Filter certain trades
fn is_valid_trade(metrics: &SwapMetrics, t: &Trade) -> bool {
    // Skip filtering for self
    if t.trader_id == "2dFanNn6mc8cb3VYsizCFjm4YXuyBhyRd597oWuxWsL4" {
        return true;
    }

    // Stablecoin address list with labels
      use std::collections::HashSet;
      let stable_coins : HashSet<String> = HashSet::from_iter([
        "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn".to_string(), // JitoSOL
        "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v".to_string(), // JupSOL
        "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So".to_string(), // mSol
        "CgnTSoL3DgY9SFHxcLj6CgCgKKoTBr6tp4CPAEWy25DE".to_string(), // cgntSOL
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(), // USDC
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".to_string(), // USDT
        "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo".to_string(), // PYUSD
    ]);

    // Skip stablecoin buys/sells
    if stable_coins.contains(&t.token_bought_mint_address.to_string()) || stable_coins.contains(&t.token_sold_mint_address.to_string()) {
        metrics.increment_skipped_stable_coins();
        return false;
    }

    // Token buy amount
    if t.token_sold_mint_address == WSOL_MINT_KEY.to_string() && (t.token_sold_amount < 0.1) {
        metrics.increment_skipped_tiny_swaps();
        return false;
    }

    // Token sell amount
    if t.token_bought_mint_address == WSOL_MINT_KEY.to_string() && (t.token_bought_amount < 0.1){
        metrics.increment_skipped_tiny_swaps();
        return false;
    }

    // Wallet blacklist
    if blacklist::is_trader_in_blacklist(&t.trader_id) {
        metrics.increment_skipped_blacklist();
        return false;
    }

    true
}

pub async fn insert_trade(trade: &Trade) -> Result<(), sqlx::Error> {
    let trade_hash = format!("{:.6}_{}_{:.6}_{}", 
        trade.token_bought_amount,
        trade.token_bought_mint_address,
        trade.token_sold_amount,
        trade.token_sold_mint_address
    );
    let mut hasher = Sha256::new();
    hasher.update(trade_hash.as_bytes());
    let result = hasher.finalize();
    let instruction_idx = ((result[0] as u32) << 24 | 
                         (result[1] as u32) << 16 | 
                         (result[2] as u32) << 8 | 
                         (result[3] as u32)) as i32;

    let pool = PG_POOL.get().expect("PG_POOL not initialized");
    let result = sqlx::query(
        r#"
        INSERT INTO trades (
            token_bought_amount,
            token_bought_mint_address,
            token_sold_amount,
            token_sold_mint_address,
            block_date,
            trader_id,
            tx_id,
            instruction_idx,
            amount_usd,
            block_slot,
            fee,
            dex,
            robot
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (tx_id, instruction_idx) DO NOTHING
        RETURNING id, tx_id, instruction_idx
        "#
    )
    .bind(trade.token_bought_amount)
    .bind(&trade.token_bought_mint_address)
    .bind(trade.token_sold_amount)
    .bind(&trade.token_sold_mint_address)
    .bind(trade.block_date)
    .bind(&trade.trader_id)
    .bind(&trade.tx_id)
    .bind(instruction_idx)
    .bind(trade.amount_usd)
    .bind(trade.block_slot as i64)
    .bind(trade.fee)
    .bind(trade.dex.to_string())
    .bind(&trade.robot)
    .fetch_optional(pool)
    .await
    .map_err(|e| e)?;

    if let Some(row) = result {
        // if trade.dex == Dex::MeteoraPools.to_string() {
        //     println!("Insert success: trade={:?} instruction_idx={}", trade, instruction_idx);
        // }
        // println!("Insert success: trade={:?} instruction_idx={}", trade, instruction_idx);
        Ok(())
        
    } else {
        println!("Record exists: trade={:?} https://solscan.io/tx/{}", 
            trade, trade.tx_id
        );
        return Err(sqlx::Error::RowNotFound);
    }

    // Report processed to mock server (disabled)
    // let _ = reqwest::Client::new()
    // .get(&format!("http://localhost:8080/report_processed_txn?txnID={}", trade.tx_id))
    // .send()
    // .await;

}

async fn update_orderbook(trade: &Trade) {
    let token_price = trade.calculate_token_sol_price();
    let token_mint = trade.get_token_mint();
    let sol_amt = trade.get_sol_amount();

    let ob = get_order_book().await;

    // Only place TP/SL copy orders when conditions are met
    if trade.is_buy_token() && sol_amt >= 0.3 && sol_amt <= 5.0 {
        let tp_price = token_price * 1.6; // 60% take profit
        let sl_price = token_price * 0.8; // 20% stop loss

        let tp_order = LimitOrder {
            wallet: trade.trader_id.to_string(), 
            token: token_mint.to_string(),
            buy_txn: trade.tx_id.to_string(),
            buy_price: token_price,
            buy_sol: sol_amt,
            buy_time: Utc::now().timestamp(),
            limit_price: OrderedFloat(tp_price),
        };
        let sl_order = LimitOrder {
            wallet: trade.trader_id.to_string(),
            token: token_mint.to_string(),
            buy_txn: trade.tx_id.to_string(), 
            buy_price: token_price,
            buy_sol: sol_amt,
            buy_time: Utc::now().timestamp(),
            limit_price: OrderedFloat(sl_price),
        };

        // GLOBAL_ORDER_BOOK.add_tp(token_mint.to_string(), tp_order);
        // GLOBAL_ORDER_BOOK.add_sl(token_mint.to_string(), sl_order);

        // Capture values in a tuple
        let closure_data = (
            token_mint.to_string(), // clone once
            tp_order.clone(),
            sl_order.clone()
        );

        // Destructure tuple
        let (token, tp, sl) = closure_data;

        let ret = ob.add_tp_async(token.clone(), tp).await;
        match ret {
            Ok(flag) => {
                // info!("add_tp_async success, token={}, result={}", token, flag);
            }
            Err(e) => {
                error!("Error add_tp_async: {}", e.to_string());
            }
        }

        let ret = ob.add_sl_async(token.clone(), sl).await;
        match ret {
            Ok(flag) => {
                // info!("add_tp_async success, token={}, result={}", token, flag);
            }
            Err(e) => {
                error!("Error add_sl_async: {}", e.to_string());
            }
        }
    }

    // Trigger orders for both buy and sell trades
    let triggered_tp_orders = ob.trigger_tp_async(
        token_mint.to_string(), 
        token_price,
        trade.tx_id.to_string()
    ).await;
    let triggered_sl_orders = ob.trigger_sl_async(
        token_mint.to_string(), 
        token_price,
        trade.tx_id.to_string()
    ).await;

    let db = ClickhouseDb::get_instance().await;

    // TP
    if let Err(e) = db.insert_orders(triggered_tp_orders.unwrap()).await {
        println!("Failed to insert TP orders: {:?}", e);
    }
    // SL
    if let Err(e) = db.insert_orders(triggered_sl_orders.unwrap()).await {
        println!("Failed to insert SL orders: {:?}", e);
    }
}

pub fn insert_trade_background(metrics: Arc<SwapMetrics>, meta: &InstructionMetadata, trade: &Trade) {
    let mut trade_clone = trade.clone();

    if is_bloom_robot(meta) {
        trade_clone.robot = "bloom".to_string();
        metrics.increment_bloom_robot();
    }

    if !is_valid_trade(&metrics, &trade) {
        return
    }

    // println!("writing trade: {:?}", trade);
    
    let txn_id = trade.tx_id.clone();

    tokio::spawn(async move {
        CopyTradingEngine::handle_trade(&trade_clone).await;
        update_orderbook(&trade_clone).await;

        let db = ClickhouseDb::get_instance().await;

        match db.insert_trade(trade_clone).await {
            Ok(_) => {
                metrics.increment_successful_swaps();
            }
            Err(e) => {
                metrics.increment_db_insert_failure();
                eprintln!("ClickHouse insert failed: tx_id={}, error={:?}", txn_id, e);
            }
        }
    });
}

pub fn write_json(data: &str, file_name: &str) -> Result<()> {
    let file = File::create(file_name)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer(writer, data)?;
    Ok(())
}

pub fn round_to_decimals(x: f64, decimals: u32) -> f64 {
    let y = 10i32.pow(decimals) as f64;
    (x * y).round() / y
}

pub fn token_to_decimals(amount: u64) -> f64 {
    amount as f64 / 10.0_f64.powf(TOKEN_DECIMALS as f64)
}

pub fn sol_to_decimals(amount: u64) -> f64 {
    amount as f64 / 10.0_f64.powf(SOL_DECIMALS as f64)
}

pub async fn get_jup_price(mint: String) -> Result<f64> {
    let url = format!(
        "https://api.jup.ag/price/v2?ids={}&vsToken=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        mint
    );

    let response = reqwest::get(&url).await?;
    let json: serde_json::Value = response.json().await?;

    // Extract price from response
    let price = json["data"][&mint]["price"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Failed to parse price"))?;

    let price = price.parse::<f64>()?;

    Ok(price)
}

pub fn may_get_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(val) => Some(val),
        Err(_) => None,
    }
}

pub fn must_get_env(key: &str) -> String {
    match std::env::var(key) {
        Ok(val) => val,
        Err(_) => panic!("{} must be set", key),
    }
}

pub async fn create_redis_pool(
    redis_url: &str,
) -> Result<bb8::Pool<RedisConnectionManager>> {
    let manager = RedisConnectionManager::new(redis_url)?;
    let pool = bb8::Pool::builder()
        .max_size(200)
        .min_idle(Some(20))
        .max_lifetime(Some(std::time::Duration::from_secs(60 * 15))) // 15 minutes
        .idle_timeout(Some(std::time::Duration::from_secs(60 * 5))) // 5 minutes
        .build(manager)
        .await?;
    Ok(pool)
}
