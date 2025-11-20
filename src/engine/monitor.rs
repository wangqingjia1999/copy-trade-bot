use tracing::info;

/// Main loop placeholder for the copy-trading worker.
/// The actual trade ingestion happens in the swap handlers; this keeps the
/// process alive and ready to mirror target wallets once trades arrive.
pub async fn pumpswap_trader() -> anyhow::Result<()> {
    info!("copy-trading engine is running; waiting for swap events to mirror");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
