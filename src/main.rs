
use solana_vntr_pumpswap_copytrader::{
    common::{config::Config, constants::RUN_MSG},
    copy_trading::CopyTradingEngine,
    engine::monitor::pumpswap_trader,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    /* Initial Settings */
    let config = Config::new().await;
    let config = config.lock().await;

    /* Running Bot */
    let run_msg = RUN_MSG;
    println!("{}", run_msg);

    CopyTradingEngine::init(config.targetlist.clone(), config.copy_trading.clone());

    pumpswap_trader().await
}
