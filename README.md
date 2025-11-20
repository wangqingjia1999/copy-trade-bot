# Solana multi-DEX copy-trading bot

> Note: This repository is provided for review when applying for a Jito shred key. The codebase has been cleaned and may not be feature-complete.
> Applicant Discord — username: lyy0.0 | userid: 845553614528708659

Lightweight copy-trading bot for Solana DEX flows (PumpSwap, Raydium, Meteora, Whirlpool, Pumpfun, etc.). The current tree prioritizes trade ingestion and copy-trade signal generation; wire in your own execution once signals are emitted.

## Overview
- **Trade ingestion**: Decoders parse swap instructions and build `Trade` structs (`src/process_swap.rs`, `src/price.rs`). PumpSwap is only one example; the pipeline is intended for multiple Solana DEXes (Raydium AMM/CLMM/CPMM/Launchpad, Meteora DLMM/Pools/DAMM, Orca Whirlpool, Pumpfun/PumpSwap)—swap handlers are pluggable per program id.
- **Copy engine**: `CopyTradingEngine` (`src/copy_trading.rs`) checks if a trade comes from a target wallet, sizes a mirrored order (min/max SOL + multiplier), and emits a `CopyTradeSignal`.
- **Hook point**: `CopyTradingEngine::handle_trade` is called for every parsed swap inside `insert_trade_background` (`src/util.rs`). Plug your executor here to actually send mirrored trades.
- **Target list**: Plain-text addresses loaded from `targetlist.txt` (`src/common/targetlist.rs`).

## Features
- Targeted copy-trading: follow only wallets listed in `targetlist.txt`.
- Dynamic sizing: multiplier plus min/max SOL budget per mirrored trade.
- Ready-to-wire execution: hook into `CopyTradingEngine::handle_trade` with your own sender (Jito/Nozomi helpers available).
- TP/SL scaffolding: optional orderbook management in `src/orderbook.rs`.
- Env-driven config: `.env` controls RPC, keys, and copy-trade knobs.

## Architecture (auditable)
- Realtime intake: geyser/RPC decoders run `process_swap.rs` ➜ normalized `Trade`; supports multiple DEX decoders (Raydium AMM/CLMM/CPMM/Launchpad, Meteora DLMM/Pools/DAMM, Orca Whirlpool, Pumpfun/PumpSwap).
- Validation & filters: `process_swap.rs` + `util.rs::is_valid_trade` (blacklist, stablecoin ignore, size thresholds).
- Copy signal: `CopyTradingEngine::plan_copy_trade` checks `targetlist.txt` and sizing rules, builds `CopyTradeSignal`.
- Hook point for execution: `CopyTradingEngine::handle_trade` (single place to send mirrored orders).
- Observability: signal logs via `tracing::info!` and metrics counters in `src/metrics.rs`.
- Persistence: trades and triggered TP/SL orders are written through `ClickhouseDb` (see `src/clickhouse.rs`) when enabled.
- Sell strategies (documented hooks, impl kept private): see `CopyExecutionPlan` in `src/copy_trading.rs` for planned MirrorSell, TimedExit, PnL bands, and Trailing exits.

## How to verify it is real copy-trading
1) Inspect the hook: open `src/copy_trading.rs` and `src/util.rs` to see signals emitted per decoded swap and where to attach execution.
2) Trace data flow: `process_swap.rs` ➜ `Trade` ➜ `insert_trade_background` ➜ `CopyTradingEngine::handle_trade`.
3) Confirm target wallet gating: `src/common/targetlist.rs` loads `targetlist.txt`; `CopyTradingEngine::plan_copy_trade` checks membership before emitting a signal.
4) Check sizing logic: multiplier + min/max SOL in `CopyTradingConfig` (env-driven).
5) Review persistence hooks: `src/clickhouse.rs` and order triggers in `src/orderbook.rs` (optional TP/SL).
6) Run locally with a throwaway `.env` and mock targetlist; observe emitted `CopyTradeSignal` logs to confirm real-time ingestion.
7) Review strategy planning: `CopyExecutionPlan` is attached to each signal (buy immediately plus multiple sell strategies) to demonstrate intent to mirror and manage exits.

## Quick start
1) Create a `.env` with RPC/auth keys and copy-trading knobs:
   ```env
   RPC_HTTP=https://your-rpc
   PRIVATE_KEY=base58_key
   YELLOWSTONE_GRPC_HTTP=https://your-geyser
   YELLOWSTONE_GRPC_TOKEN=token
   COPY_TRADING_ENABLED=true
   COPY_TRADE_MULTIPLIER=1.0
   COPY_TRADE_MIN_SOL=0.0
   COPY_TRADE_MAX_SOL=10.0
   ```
2) Add target wallets to `targetlist.txt` (one per line).
3) Run the binary:
   ```bash
   cargo run
   ```
   The process stays alive and logs copy-trade signals; execution is not yet wired.

## Wire in execution
- Add your executor call inside `CopyTradingEngine::handle_trade` when a `CopyTradeSignal` is produced.
- You can reuse the transaction helpers in `src/core/tx.rs` (Jito/Nozomi examples) or your own RPC sender.
- Signals include: `token_mint`, `is_buy`, `target_sol`, `reference_price`, `dex`, `source_tx`, `source_trader`, and observed token/SOL amounts.

## Key files
- `src/copy_trading.rs`: Copy-trade config, sizing, and signal generation.
- `src/util.rs`: Ingestion hook calling the copy engine on each swap.
- `src/process_swap.rs`: Swap decoding and `Trade` construction.
- `src/orderbook.rs`: TP/SL order management (optional for copy execution).
- `src/common/config.rs`: Loads env vars and passes copy-trading config into the engine.

## Notes
- Network access may be restricted in some environments; dependency resolution can fail without crates.io.
- Many carbon* decoder crates are pinned to the `0.11` line in `Cargo.toml`; adjust versions or sources if you use a private mirror.

## Core logic at a glance
- Swap decoded into `Trade` (`src/process_swap.rs`, `src/price.rs`).
- `insert_trade_background` (`src/util.rs`) runs per trade: orderbook/DB paths, then calls `CopyTradingEngine::handle_trade`.
- `CopyTradingEngine` (`src/copy_trading.rs`) checks target wallets, applies multiplier + min/max SOL sizing, emits `CopyTradeSignal`.
- Plug your executor inside `CopyTradingEngine::handle_trade` when a signal exists to actually place mirrored trades (helpers in `src/core/tx.rs` or your own).

### Diagram
```
[Swap Tx decoded]
       |
       v
  process_swap.rs
       |
       v
  Trade built
       |
       v
insert_trade_background (util.rs)
  |   \
  |    --> Orderbook/DB (optional)
  v
CopyTradingEngine::handle_trade
  |
  |-- targetlist + sizing
  |
  +-- CopyTradeSignal
          |
          v
    YOUR EXECUTOR HERE
    (send mirrored buy/sell)
```
