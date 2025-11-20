use std::fmt;

use tokio::sync::OnceCell;
use tracing::info;

use crate::{
    common::targetlist::Targetlist,
    price::Trade,
};

/// High-level sell strategy types investors can configure offline.
/// The concrete execution is intentionally left unimplemented for security/IP reasons.
#[derive(Clone, Debug)]
pub enum SellStrategyKind {
    /// Mirror the source trader's sells 1:1.
    MirrorSell,
    /// Time-based exit (e.g., sell after N seconds/minutes).
    TimedExit,
    /// Exit based on PnL thresholds (take profit / stop loss bands).
    PnlBands,
    /// Trailing exit: follow rising price, exit on pullback.
    Trailing,
}

#[derive(Clone, Debug)]
pub struct SellStrategyPlan {
    pub kind: SellStrategyKind,
    /// Human-readable note for auditors; actual execution lives in a private service.
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct CopyExecutionPlan {
    pub buy_immediately: bool,
    pub buy_note: String,
    pub sell_strategies: Vec<SellStrategyPlan>,
}

#[derive(Clone, Debug)]
pub struct CopyTradingConfig {
    pub enabled: bool,
    pub multiplier: f64,
    pub min_sol: f64,
    pub max_sol: f64,
}

impl CopyTradingConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            multiplier: 1.0,
            min_sol: 0.0,
            max_sol: 0.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CopyTradeSignal {
    pub token_mint: String,
    pub is_buy: bool,
    pub target_sol: f64,
    pub reference_price: f64,
    pub source_tx: String,
    pub source_trader: String,
    pub dex: String,
    pub observed_token: f64,
    pub observed_sol: f64,
    pub observed_at: i64,
    pub execution_plan: CopyExecutionPlan,
}

impl fmt::Display for CopyTradeSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[copy-trade] {} {} {:.4} SOL @ {:.8} (token {:.4}) from {} on {} | tx {}",
            if self.is_buy { "BUY" } else { "SELL" },
            self.token_mint,
            self.target_sol,
            self.reference_price,
            self.observed_token,
            self.source_trader,
            self.dex,
            self.source_tx,
        )
    }
}

pub struct CopyTradingEngine {
    config: CopyTradingConfig,
    targetlist: Targetlist,
}

static COPY_ENGINE: OnceCell<CopyTradingEngine> = OnceCell::const_new();

impl CopyTradingEngine {
    pub fn init(targetlist: Targetlist, config: CopyTradingConfig) {
        // Ignore repeated init attempts to keep first successful configuration.
        let _ = COPY_ENGINE.set(Self { targetlist, config });
    }

    pub fn global() -> Option<&'static CopyTradingEngine> {
        COPY_ENGINE.get()
    }

    pub fn plan_copy_trade(&self, trade: &Trade) -> Option<CopyTradeSignal> {
        if !self.config.enabled {
            return None;
        }

        if !self.targetlist.is_listed_on_target(&trade.trader_id) {
            return None;
        }

        let observed_sol = trade.get_sol_amount();
        let mut target_sol = observed_sol * self.config.multiplier;
        target_sol = target_sol.clamp(self.config.min_sol, self.config.max_sol);

        if target_sol <= 0.0 {
            return None;
        }

        // Build an auditable execution plan (not executed here):
        // - Buy immediately with computed target_sol.
        // - Prepare multiple sell strategies (mirror, time-based, PnL/Trailing).
        let execution_plan = CopyExecutionPlan {
            buy_immediately: true,
            buy_note: "Mirror BUY as soon as source swap is observed.".to_string(),
            sell_strategies: vec![
                SellStrategyPlan {
                    kind: SellStrategyKind::MirrorSell,
                    note: "When source trader sells, submit a proportional SELL.".to_string(),
                },
                SellStrategyPlan {
                    kind: SellStrategyKind::TimedExit,
                    note: "Schedule a SELL after a configured horizon if nothing else triggers.".to_string(),
                },
                SellStrategyPlan {
                    kind: SellStrategyKind::PnlBands,
                    note: "Set TP/SL bands based on observed entry price; exit on hit.".to_string(),
                },
                SellStrategyPlan {
                    kind: SellStrategyKind::Trailing,
                    note: "Enable trailing stop to capture upside and exit on pullback.".to_string(),
                },
            ],
        };

        Some(CopyTradeSignal {
            token_mint: trade.get_token_mint().to_string(),
            is_buy: trade.is_buy_token(),
            target_sol,
            reference_price: trade.calculate_token_sol_price(),
            source_tx: trade.tx_id.clone(),
            source_trader: trade.trader_id.clone(),
            dex: trade.dex.clone(),
            observed_token: trade.get_token_amount(),
            observed_sol,
            observed_at: trade.block_date,
            execution_plan,
        })
    }

    pub async fn handle_trade(trade: &Trade) {
        if let Some(engine) = COPY_ENGINE.get() {
            if let Some(signal) = engine.plan_copy_trade(trade) {
                // Auditable log for reviewers: shows BUY intent and planned SELL strategies.
                info!(
                    "{} | plan={:?}",
                    signal,
                    signal.execution_plan.sell_strategies
                );

                // Placeholder: integrate with private executor
                // executor.enqueue_buy_and_sell_strategies(signal, signal.execution_plan)
                // The actual order placement code is kept private; hooks remain here for auditors.
            }
        }
    }
}
