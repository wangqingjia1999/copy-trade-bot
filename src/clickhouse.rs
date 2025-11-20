use std::{env, sync::Arc, time::Duration};
use std::cmp::max;
use crate::constants::WSOL_MINT_KEY;
use crate::orderbook::{LimitOrder, TriggeredOrder};
use crate::price::{PriceUpdate, Trade};
use crate::sol_price::get_sol_price;
use anyhow::{Context, Result};
use clickhouse::inserter::Inserter;
use clickhouse::Client;
use tokio::sync::{OnceCell, RwLock};
use tracing::{debug, info};

pub struct ClickhouseDb {
    client: Client,
    trade_inserter: Option<Arc<RwLock<Inserter<Trade>>>>,
    triggered_order_inserter: Option<Arc<RwLock<Inserter<OrderDB>>>>,
    is_initialized: bool,
}

#[derive(Debug, Clone, PartialEq, clickhouse::Row, serde::Serialize)]
struct OrderDB {
    pub wallet: String,
    pub buy_txn: String,
    pub buy_price: f64,
    pub buy_sol: f64,
    pub buy_time: i64,
    pub limit_price: f64,
    pub trigger_txn: String,
    pub trigger_price: f64,
    pub trigger_time: i64,
    pub token: String,
    pub is_tp: bool,
    pub uuid: String,
}

static INSTANCE: OnceCell<ClickhouseDb> = OnceCell::const_new();

impl ClickhouseDb {
    pub async fn get_instance() -> &'static ClickhouseDb {
        match INSTANCE
            .get_or_try_init(|| async {
                let mut db = ClickhouseDb::new(
                    "http://localhost:8123",
                    "qwer",
                    "default",
                    "solana",
                );
                db.initialize().await?;
                Ok::<ClickhouseDb, anyhow::Error>(db)
            })
            .await
        {
            Ok(instance) => instance,
            Err(err) => {
                eprintln!("❌ Failed to initialize ClickHouse instance: {:?}", err);
                panic!("ClickHouse init failed: {}", err);
            }
        }
    }

    pub fn new(
        database_url: &str,
        password: &str,
        user: &str,
        database: &str,
    ) -> Self {
        let client = Client::default()
            .with_url(database_url)
            .with_password(password)
            .with_user(user)
            .with_database(database);

        info!("Connecting to ClickHouse at {}", database_url);
        Self {
            client,
            trade_inserter: None,
            triggered_order_inserter: None,
            is_initialized: false,
        }
    }

    pub async fn initialize(&mut self) -> Result<()> {
        // Create database if missing
        self.client
            .query("CREATE DATABASE IF NOT EXISTS solana")
            .execute()
            .await
            .context("Failed to create database")?;

        const CREATE_TABLE_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS solana.trades (
            token_bought_amount Float64,
            token_bought_mint_address String,
            token_sold_amount Float64,
            token_sold_mint_address String,
            block_date Int64,
            trader_id String,
            tx_id String,
            instruction_idx Int32,
            amount_usd Float64,
            block_slot UInt64,
            fee Float64,
            dex String,
            robot String,
            uuid UUID DEFAULT generateUUIDv4()
        ) ENGINE = MergeTree()
        ORDER BY (tx_id, instruction_idx)
        TTL toDateTime(block_date) + INTERVAL 30 DAY
        SETTINGS index_granularity = 8192;
        "#;

        debug!("initializing clickhouse");

        self.client
            .query(CREATE_TABLE_SQL)
            .execute()
            .await
            .context("Failed to create ClickHouse table")?;

        let inserter = self.client
            .inserter::<Trade>("trades")
            .context("Failed to create inserter")?
            .with_max_rows(1000);

        self.trade_inserter = Some(Arc::new(RwLock::new(inserter)));

        const CREATE_TRIGGERED_ORDER_TABLE_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS solana.orders (
            wallet String,
            buy_txn String,
            buy_price Float64,
            buy_sol Float64,
            buy_time Int64,
            limit_price Float64,
            trigger_txn String,
            trigger_price Float64,
            trigger_time Int64,
            token String,
            is_tp Bool,
            uuid String,
        ) ENGINE = MergeTree()
        ORDER BY (wallet, buy_time)
        TTL toDateTime(trigger_time) + INTERVAL 30 DAY
        SETTINGS index_granularity = 8192;
        "#;

        self.client
            .query(CREATE_TRIGGERED_ORDER_TABLE_SQL)
            .execute()
            .await
            .context("Failed to create triggered orders table")?;

        let triggered_order_inserter = self.client
            .inserter::<OrderDB>("orders")
            .context("Failed to create triggered orders inserter")?
            .with_max_rows(10);

        self.triggered_order_inserter = Some(Arc::new(RwLock::new(triggered_order_inserter)));

        self.is_initialized = true;
        Ok(())
    }

    pub async fn insert_trade(&self, trade: Trade) -> Result<()> {
        let inserter_lock = self.trade_inserter.as_ref().ok_or_else(|| anyhow::anyhow!("Inserter not initialized"))?;
        let mut inserter = inserter_lock.write().await;

        // Insert records
        inserter
            .write(&trade)
            .context("Failed to write trade into ClickHouse inserter")?;

        inserter.commit().await?;

        Ok(())
    }

    pub async fn insert_orders(&self, orders: Vec<TriggeredOrder>) -> Result<()> {
        let inserter_lock = self.triggered_order_inserter.as_ref().ok_or_else(|| anyhow::anyhow!("Inserter not initialized"))?;
        let mut inserter = inserter_lock.write().await;

        for order in orders {
            let order_db = OrderDB {
                wallet: order.order.wallet,
                buy_txn: order.order.buy_txn,
                buy_price: order.order.buy_price,
                buy_sol: order.order.buy_sol,
                buy_time: order.order.buy_time,
                limit_price: order.order.limit_price.into(),
                trigger_txn: order.trigger_txn,
                trigger_price: order.trigger_price,
                trigger_time: order.trigger_time,
                token: order.token,
                is_tp: order.is_tp,
                uuid: order.uuid,
            };
            inserter
                .write(&order_db)
                .context("Failed to write order into ClickHouse inserter")?;
        }

        inserter.commit().await?;
        Ok(())
    }
}
