use chrono::{Timelike, Utc};
use once_cell::sync::Lazy;
use ordered_float::OrderedFloat;
use redis::{aio::MultiplexedConnection, AsyncCommands};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, thread::Builder, time::Duration};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::clickhouse::ClickhouseDb;

// TTL seconds
const TTL_SECONDS: i64 = 300;

static GLOBAL_ORDER_BOOK: OnceCell<Arc<OrderBook>> = OnceCell::const_new();

pub async fn get_order_book() -> Arc<OrderBook> {
    GLOBAL_ORDER_BOOK
        .get_or_init(|| async {
            // Start watcher if needed
            OrderBook::new().await
        })
        .await
        .clone()
}

// ====== Data structures ======
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitOrder {
    pub wallet: String,
    pub token: String,
    pub buy_txn: String,
    pub buy_price: f64,
    pub buy_sol: f64,
    pub buy_time: i64,
    pub limit_price: OrderedFloat<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggeredOrder {
    pub order: LimitOrder,
    pub trigger_txn: String,
    pub trigger_price: f64,
    pub trigger_time: i64,
    pub is_tp: bool,
    pub token: String,
    pub uuid: String,
}

// ====== OrderBook: async internal, sync external ======
pub struct OrderBook {
    conn: MultiplexedConnection,
}

impl OrderBook {
    pub async fn new() -> Arc<Self> {
        let client = redis::Client::open("redis://127.0.0.1/").expect("redis");
        let conn = client
            .get_multiplexed_tokio_connection()
            .await
            .expect("redis conn");

        // Build instance first
        let ob = Arc::new(Self { conn });

        // Launch timeout watcher
        ob.start_timeout_watcher_simple();

        // Return instance
        ob
    }
    
    pub async fn add_tp_async(
        self: &Arc<Self>,
        token: String,
        order: LimitOrder,
    ) -> redis::RedisResult<bool> {
        self.add_order_async(token, order, true).await
    }

    pub async fn add_sl_async(
        self: &Arc<Self>,
        token: String,
        order: LimitOrder,
    ) -> redis::RedisResult<bool> {
        self.add_order_async(token, order, false).await
    }

    async fn add_order_async(
        self: &Arc<Self>,
        token: String,
        order: LimitOrder,
        is_tp: bool,
    ) -> redis::RedisResult<bool> {
        let mut conn = self.conn.clone();

        // ZSET key
        let zkey = if is_tp {
            format!("tp:{}", token)
        } else {
            format!("sl:{}", token)
        };

        let field = format!("{}:{}:{}", token, order.wallet, if is_tp { "1" } else { "0" });

        // Expire key: after 10 minutes mark as SL
        let expire_key = if is_tp {
            format!("expire:tp:{}:{}", token, order.wallet)
        } else {
            format!("expire:sl:{}:{}", token, order.wallet)
        };

        // Serialize order, ZSET score
        let member_json = serde_json::to_string(&order).unwrap();
        let score = order.limit_price.0.to_string();

        // Current timestamp (seconds)
        let now_sec = chrono::Utc::now().timestamp();
        // Expire timestamp (seconds)
        let expire_at_sec = now_sec + TTL_SECONDS;

        // zkey: tp/sl:token score -> member             | price lookup
        // tkey: wallet_triggers <token:wallet:tp/sl, ?> | remove on trigger (unused)
        // ekey: expire:tp/sl:token:wallet -> expire_sec | track expiry, 30d TTL

        let lua = r#"
            local zkey = KEYS[1]
            local ekey = KEYS[2]

            local field = ARGV[1]
            local member = ARGV[2]
            local score  = tonumber(ARGV[3])
            local expire_at_sec = tonumber(ARGV[4])

            -- If ekey exists, SET ... NX returns nil; set with TTL when missing
            local ok = redis.call('SET', ekey, tostring(expire_at_sec), 'NX', 'EX', tostring(2592000))
            if not ok then
                return 0  -- already placed
            end

            redis.call('ZADD', zkey, score, member)

            return 1
        "#;

        let added: i32 = redis::cmd("EVAL")
            .arg(lua)
            .arg(2)           // KEYS count
            .arg(zkey)
            .arg(expire_key)
            .arg(field)
            .arg(member_json)
            .arg(score)
            .arg(expire_at_sec.to_string()) // pass TTL
            .query_async(&mut conn)
            .await?;

        Ok(added == 1)
    }

    /// Start timeout watcher thread (treat all expirations as SL; no throttling/callbacks)
    pub fn start_timeout_watcher_simple(self: &Arc<Self>) {
        let ob = Arc::clone(self);

         // Log on startup to confirm invocation
        println!(
            "[timeout watcher] qqq starting…"
        );

        Builder::new()
        .name("orderbook-timeout-watcher".into())
        .spawn(move || {
                // Dedicated current-thread runtime to isolate from main runtime
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build watcher runtime");

                rt.block_on(async move {
                    let mut ticker = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        ticker.tick().await;

                        match ob.check_expired_orders().await {
                            Ok(orders) => {
                                let count = orders.len();
                                if count == 0 {
                                    println!("[timeout watcher] no expired orders");
                                    continue;
                                }

                                #[cfg(debug_assertions)]
                                {
                                    println!("[timeout watcher] inserting {} expired orders: {:#?}", count, orders);
                                }

                                let db = ClickhouseDb::get_instance().await;

                                // Write orders via global DB singleton
                                if let Err(e) = db.insert_orders(orders).await {
                                    eprintln!("[timeout watcher] failed to insert expired orders: {e:?}");
                                } else {
                                    println!("[timeout watcher] inserted {} expired orders", count);
                                }
                            }
                            Err(e) => {
                                eprintln!("[timeout watcher] check_expired_orders error: {e}");
                            }
                        }
                    }
                });
            })
            .expect("spawn timeout watcher thread");
    }

    pub async fn check_expired_orders(
        self: &Arc<Self>,
    ) -> redis::RedisResult<Vec<TriggeredOrder>> {
        use redis::AsyncCommands;

        let mut conn = self.conn.clone();
        let mut out = Vec::new();

        let now_sec = chrono::Utc::now().timestamp();
        const TTL_SEC: usize = 2_592_000; // 30 days

        // SCAN all expire:*
        let mut cursor: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) =
                redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH").arg("expire:*")
                    .arg("COUNT").arg(1024)
                    .query_async(&mut conn).await?;

            for expire_key in keys {
                let expire_at_sec: Option<i64> = conn.get(&expire_key).await?;
                let Some(expire_at_sec) = expire_at_sec else { continue; };

                // Already processed
                if expire_at_sec < 0 {
                    continue;
                }

                if now_sec < expire_at_sec {
                    // Not expired yet
                    continue;
                }

                // Parse expire:{tp|sl}:{token}:{wallet}
                let parts: Vec<&str> = expire_key.split(':').collect();
                if parts.len() != 4 { 
                    let _: () = conn.del(&expire_key).await.unwrap_or(()); // invalid structure, clean up
                    println!("[check_expired_orders] invalid expire key, cleaned: {}", expire_key);
                    continue; 
                }
                let side   = parts[1]; // "tp" | "sl"
                let token  = parts[2].to_string();
                let wallet = parts[3].to_string();

                let zkey   = format!("{}:{}", side, token);
                
                let tp_ekey = format!("expire:tp:{}:{}", token, wallet);
                let sl_ekey = format!("expire:sl:{}:{}", token, wallet);

                // Batch ZSCAN to avoid full fetch
                let mut zcursor: u64 = 0;
                let mut found_this_wallet = false;

                loop {
                    let (znext, chunk): (u64, Vec<(String, f64)>) =
                        redis::cmd("ZSCAN")
                            .arg(&zkey)
                            .arg(zcursor)
                            .arg("COUNT").arg(256)
                            .query_async(&mut conn).await
                            .unwrap_or((0, Vec::new()));

                    for (member_json, _score) in chunk {
                        // Handle nested JSON
                        let parsed = serde_json::from_str::<LimitOrder>(&member_json)
                            .or_else(|_| serde_json::from_str::<String>(&member_json)
                                .and_then(|s| serde_json::from_str::<LimitOrder>(&s)));
                        let order = match parsed {
                            Ok(o) => o,
                            Err(_) => {
                                // Drop bad data
                                let _: () = conn.zrem(&zkey, &member_json).await.unwrap_or(());
                                continue;
                            }
                        };

                        // Timeout only if both sides marked
                        let tp_marked: bool = conn.get::<_, Option<i64>>(&tp_ekey).await?
                            .map_or(false, |v| v == -1);
                        let sl_marked: bool = conn.get::<_, Option<i64>>(&sl_ekey).await?
                            .map_or(false, |v| v == -1);

                        if !tp_marked && sl_marked {
                            println!("BUG: {} TP not triggered but SL triggered", member_json);
                        }
                        if tp_marked && !sl_marked {
                            println!("BUG: {} TP triggered but SL not triggered", member_json);
                        }

                        if !tp_marked || !sl_marked {
                            continue;
                        }

                        // Found matching wallet
                        if order.wallet == wallet {
                            out.push(TriggeredOrder {
                                uuid: Uuid::new_v4().to_string(),
                                order: order.clone(),
                                trigger_txn: "timeout".into(),
                                trigger_price: order.limit_price.0,
                                trigger_time: chrono::Utc::now().timestamp(),
                                token: order.token,
                                is_tp: false,
                            });

                            // Remove exact member
                            let _: () = conn.zrem(&zkey, &member_json).await.unwrap_or(());

                            // Mark both sides with TTL
                            let _: () = redis::cmd("SET").arg(&tp_ekey).arg(-1)
                                .arg("EX").arg(TTL_SEC)
                                .query_async(&mut conn).await.unwrap_or(());

                            let _: () = redis::cmd("SET").arg(&sl_ekey).arg(-1)
                                .arg("EX").arg(TTL_SEC)
                                .query_async(&mut conn).await.unwrap_or(());

                            found_this_wallet = true;
                            break; // one order per side; stop scanning once found
                        }
                    }

                    zcursor = znext;
                    if zcursor == 0 || found_this_wallet { break; }
                }
            }

            cursor = next;
            if cursor == 0 { break; }
        }
        Ok(out)
    }

    // Trigger TP: score <= market; if SL not marked, ZREM and mark both
    pub async fn trigger_tp_async(
        &self,
        token: String,
        market_price: f64,
        trigger_txn: String,
    ) -> redis::RedisResult<Vec<TriggeredOrder>> {
        let mut conn = self.conn.clone();
        let zkey = format!("tp:{}", token);
        let invalid_mark = -1;

        // Lua: ZRANGEBYSCORE decode member; if SL not marked, remove and mark both sides
        let lua = r#"
            local zkey = KEYS[1]
            local token = ARGV[1]
            local max_price = ARGV[2]
            local invalid_mark = ARGV[3]
            local ttl = 2592000  -- 30 days

            local members = redis.call('ZRANGEBYSCORE', zkey, '-inf', max_price)
            local out = {}
            for _, mem in ipairs(members) do
                local ok, obj = pcall(cjson.decode, mem)

                if ok and type(obj) == 'table' and obj.wallet then
                    local wallet = tostring(obj.wallet)

                    local tp_expire = "expire:tp:" .. token .. ":" .. wallet
                    local sl_expire = "expire:sl:" .. token .. ":" .. wallet

                    local tp_expire_val = redis.call('GET', tp_expire)
                    local sl_expire_val = redis.call('GET', sl_expire)

                    -- string compare to avoid tonumber(nil)
                    local tp_marked = (tp_expire_val ~= false and tp_expire_val ~= nil and tp_expire_val == invalid_mark)
                    local sl_marked = (sl_expire_val ~= false and sl_expire_val ~= nil and sl_expire_val == invalid_mark)

                    if not (tp_marked and sl_marked) then
                        -- Only if TP not marked, treat as first trigger
                        if not tp_marked then
                            redis.call('ZREM', zkey, mem)
                            out[#out + 1] = mem
                        end

                        -- Mark both sides and set TTL
                        redis.call('SET', tp_expire, invalid_mark, 'EX', ttl)
                        redis.call('SET', sl_expire, invalid_mark, 'EX', ttl)
                    end
                else
                    -- Remove invalid JSON
                    redis.call('ZREM', zkey, mem)
                end
            end

            return out
        "#;

        let raws: Vec<String> = redis::cmd("EVAL")
            .arg(lua)
            .arg(1)
            .arg(zkey)
            .arg(token.clone())
            .arg(market_price.to_string())
            .arg(invalid_mark)
            .query_async(&mut conn)
            .await?;

        // Build TriggeredOrder
        let mut out = Vec::new();
        for s in raws {
            let order: LimitOrder = serde_json::from_str(&s).map_err(|e| {
                redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "parse order json",
                    e.to_string(),
                ))
            })?;
            out.push(TriggeredOrder {
                uuid: Uuid::new_v4().to_string(),
                order,
                token: token.clone(),
                trigger_txn: trigger_txn.clone(),
                trigger_price: market_price,
                trigger_time: Utc::now().timestamp(),
                is_tp: true,
            });
        }
        Ok(out)
    }

    // Trigger SL: score >= market; if TP not marked, ZREM and mark both
    pub async fn trigger_sl_async(
        &self,
        token: String,
        market_price: f64,
        trigger_txn: String,
    ) -> redis::RedisResult<Vec<TriggeredOrder>> {
        let mut conn = self.conn.clone();
        let zkey = format!("sl:{}", token);
        let invalid_mark = -1;

        let lua = r#"
            local zkey = KEYS[1]
            local token = ARGV[1]
            local min_price = ARGV[2]
            local invalid_mark = ARGV[3]
            local ttl = 2592000  -- 30 days

            local members = redis.call('ZRANGEBYSCORE', zkey, min_price, '+inf')
            local out = {}

            for _, mem in ipairs(members) do
                local ok, obj = pcall(cjson.decode, mem)

                if ok and type(obj) == 'table' and obj.wallet then
                    local wallet = tostring(obj.wallet)

                    local tp_expire = "expire:tp:" .. token .. ":" .. wallet
                    local sl_expire = "expire:sl:" .. token .. ":" .. wallet

                    local tp_expire_val = redis.call('GET', tp_expire)
                    local sl_expire_val = redis.call('GET', sl_expire)

                    -- string comparison to avoid tonumber(nil)
                    local tp_marked = (tp_expire_val ~= false and tp_expire_val ~= nil and tp_expire_val == invalid_mark)
                    local sl_marked = (sl_expire_val ~= false and sl_expire_val ~= nil and sl_expire_val == invalid_mark)

                    if not (tp_marked and sl_marked) then
                        -- Only if SL not marked, treat as first trigger
                        if not sl_marked then
                            redis.call('ZREM', zkey, mem)
                            out[#out + 1] = mem
                        end

                        -- Mark both sides and set TTL
                        redis.call('SET', tp_expire, invalid_mark, 'EX', ttl)
                        redis.call('SET', sl_expire, invalid_mark, 'EX', ttl)
                    end
                else
                    -- Remove invalid JSON
                    redis.call('ZREM', zkey, mem)
                end
            end

            return out
        "#;

        let raws: Vec<String> = redis::cmd("EVAL")
            .arg(lua)
            .arg(1)
            .arg(zkey)
            .arg(token.clone())
            .arg(market_price.to_string())
            .arg(invalid_mark)
            .query_async(&mut conn)
            .await?;

        let mut out = Vec::new();
        for s in raws {
            let order: LimitOrder = serde_json::from_str(&s).map_err(|e| {
                redis::RedisError::from((
                    redis::ErrorKind::TypeError,
                    "parse order json",
                    e.to_string(),
                ))
            })?;
            out.push(TriggeredOrder {
                uuid: Uuid::new_v4().to_string(),
                order,
                token: token.clone(),
                trigger_txn: trigger_txn.clone(),
                trigger_price: market_price,
                trigger_time: Utc::now().timestamp(),
                is_tp: false,
            });
        }
        Ok(out)
    }
}
