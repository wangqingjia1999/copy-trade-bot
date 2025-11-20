use crate::{
    constants::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
    util::make_rpc_client,
};
use anyhow::{Context, Result};
use mpl_token_metadata::accounts::Metadata;
use serde::{Deserialize, Serialize};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use spl_token_2022::{extension::StateWithExtensions, state::Mint};
use std::{str::FromStr};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MplTokenMetadata {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub ipfs_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SplTokenMetadata {
    pub mint_authority: Option<String>,
    pub supply: u64,
    pub decimals: u8,
    pub is_initialized: bool,
    pub freeze_authority: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenMetadata {
    pub mint: String,
    pub mpl: MplTokenMetadata,
    pub spl: SplTokenMetadata,
}

fn extract_ipfs_cid(uri: &str) -> Option<String> {
    if uri.starts_with("ipfs://") {
        Some(uri.replace("ipfs://", ""))
    } else if uri.contains("/ipfs/") {
        uri.split("/ipfs/").nth(1).map(|s| s.to_string())
    } else {
        None
    }
}

fn convert_ipfs_uri(uri: &str) -> String {
    if let Some(cid) = extract_ipfs_cid(uri) {
        format!("https://ipfs.io/ipfs/{}", cid)
    } else {
        uri.to_string()
    }
}

impl TokenMetadata {
    pub async fn fetch_by_mint(mint: &str) -> Result<Self> {
        let mpl_metadata = TokenMetadata::fetch_mpl_by_mint(mint)
            .await
            .unwrap_or_default();
        let spl_metadata = TokenMetadata::fetch_spl_by_mint(mint).await?;

        Ok(TokenMetadata {
            mint: mint.to_string(),
            mpl: mpl_metadata,
            spl: spl_metadata,
        })
    }

    // Input mint address, fetch via RPC, auto-detect SPL vs 2022 token, parse on-chain metadata, and return token metadata.
    pub async fn fetch_spl_by_mint(mint: &str) -> Result<SplTokenMetadata> {
        let rpc_client = make_rpc_client()?;
        let token_pubkey = Pubkey::from_str(mint)?;
        let token_account = rpc_client
            .get_account_with_commitment(
                &token_pubkey,
                CommitmentConfig::processed(),
            )
            .await
            .context("failed to get token account")?;

        let account = token_account.value.context("Token account not found")?;
        let data = &account.data;

        let token_data = match account.owner.to_string().as_ref() {
            TOKEN_PROGRAM_ID => {
                Mint::unpack(&data).expect("Failed to unpack mint")
            }
            TOKEN_2022_PROGRAM_ID => {
                let state_with_extensions =
                    StateWithExtensions::<Mint>::unpack(&data)
                        .expect("failed to unpack Token-2022 mint data");
                state_with_extensions.base
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Unknown token program owner: {}",
                    account.owner
                ));
            }
        };

        debug!(mint, "spl metadata fetch ok");

        Ok(SplTokenMetadata {
            mint_authority: token_data
                .mint_authority
                .map(|p| p.to_string())
                .into(),
            supply: token_data.supply,
            decimals: token_data.decimals,
            is_initialized: token_data.is_initialized,
            freeze_authority: token_data
                .freeze_authority
                .map(|p| p.to_string())
                .into(),
        })
    }

    pub async fn fetch_mpl_by_mint(mint: &str) -> Result<MplTokenMetadata> {
        let rpc_client =
            make_rpc_client().context("failed to make rpc client")?;
        let token_pubkey =
            Pubkey::from_str(mint).context("failed to parse mint")?;

        // Find metadata PDA
        let (metadata_pubkey, _) = Metadata::find_pda(&token_pubkey);
        debug!(
            mint,
            metadata_pubkey = metadata_pubkey.to_string(),
            "attempting to fetch MPL metadata"
        );

        // Get metadata account data
        let metadata_account = rpc_client
            .get_account_with_commitment(
                &metadata_pubkey,
                CommitmentConfig::processed(),
            )
            .await
            .context(format!(
                "failed to get metadata account: {}",
                metadata_pubkey
            ))?;

        let data = metadata_account
            .value
            .context(format!(
                "Metadata account not found: token: {} mpl pda: {}",
                token_pubkey, metadata_pubkey
            ))?
            .data;

        debug!(mint, data_len = data.len(), "got metadata account data");

        let metadata = match Metadata::from_bytes(&data) {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    mint,
                    error = e.to_string(),
                    "failed to parse metadata, trying alternative parsing"
                );
                return Err(e.into());
            }
        };

        debug!(
            mint,
            name = metadata.name,
            symbol = metadata.symbol,
            uri = metadata.uri,
            "parsed metadata successfully"
        );

        let uri = convert_ipfs_uri(&metadata.uri)
            .trim_matches(char::from(0))
            .to_string();

        // Create base token metadata
        let mut token_metadata = MplTokenMetadata {
            name: metadata.name.trim_matches(char::from(0)).to_string(),
            symbol: metadata.symbol.trim_matches(char::from(0)).to_string(),
            uri: uri.clone(),
            ipfs_metadata: None,
        };

        // Fetch IPFS metadata if available
        let client = reqwest::Client::new();
        if let Ok(response) = client.get(&uri).send().await {
            if let Ok(ipfs_metadata) =
                response.json::<serde_json::Value>().await
            {
                debug!(mint, uri, "ipfs fetch ok");
                token_metadata.ipfs_metadata = Some(ipfs_metadata);
            } else {
                warn!(mint, uri, "ipfs response not json");
            }
        } else {
            warn!(mint, uri, "ipfs fetch failed");
        }

        Ok(token_metadata)
    }
}
