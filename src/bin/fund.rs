//! Funds the configured accounts on an anvil fork with ETH and the configured tokens.

use std::str::FromStr;

use alloy::providers::ProviderBuilder;
use arbitrage::chain::evm::accounts::signers;
use arbitrage::chain::evm::config::EvmConfig;
use arbitrage::chain::evm::env_var;
use arbitrage::chain::evm::fund::fund;
use arbitrage::config::NetworkConfig;
use arbitrage::error::Error;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Network funded when `NETWORK` is unset.
const DEFAULT_NETWORK: &str = "ethereum";
/// Log filter used when `RUST_LOG` is unset.
const DEFAULT_LOG: &str = "info,alloy_transport_http=warn";

#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenvy::dotenv().ok();
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
  tracing_subscriber::fmt().with_env_filter(filter).init();
  let network = std::env::var("NETWORK").unwrap_or_else(|_| DEFAULT_NETWORK.into());
  let cfg = NetworkConfig::load(&network)?;
  let evm = EvmConfig::parse(&cfg)?;
  let fork = evm
    .fork
    .as_ref()
    .ok_or_else(|| Error::Config("adapter.fork: missing, nothing to fund".into()))?;
  let treasury = alloy::primitives::Address::from_str(&cfg.treasury)
    .map_err(|e| Error::Config(format!("treasury: {e}")))?;
  let accounts: Vec<_> = signers(&cfg.accounts)?
    .iter()
    .map(|signer| signer.address())
    .collect();
  let provider = ProviderBuilder::new().connect(&env_var("RPC_URL")?).await?;
  for funded in fund(&provider, fork, treasury, &accounts).await? {
    info!(account = %funded.address, eth = %funded.eth, "funded");
    for (token, held) in &funded.tokens {
      info!(account = %funded.address, %token, balance = %held, "token balance");
    }
  }
  Ok(())
}
