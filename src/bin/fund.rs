//! Funds the configured accounts on an anvil fork with ETH and the configured tokens.

use alloy::providers::ProviderBuilder;
use arbitrage::chain::evm::accounts::signers;
use arbitrage::chain::evm::config::EvmConfig;
use arbitrage::chain::evm::fund::fund;
use arbitrage::chain::evm::{env_var, parse_address};
use arbitrage::config::NetworkConfig;
use arbitrage::error::Error;
use arbitrage::redact::{self, Redacting};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Network funded when `NETWORK` is unset.
const DEFAULT_NETWORK: &str = "ethereum";
/// Log filter used when `RUST_LOG` is unset.
const DEFAULT_LOG: &str = "info,alloy_transport_http=warn";

/// Funds every configured account of `NETWORK` and logs what each holds afterwards.
async fn fund_accounts() -> eyre::Result<()> {
  let network = std::env::var("NETWORK").unwrap_or_else(|_| DEFAULT_NETWORK.into());
  let cfg = NetworkConfig::load(&network)?;
  let evm = EvmConfig::parse(&cfg)?;
  let fork = evm
    .fork
    .as_ref()
    .ok_or_else(|| Error::Config("adapter.fork: missing, nothing to fund".into()))?;
  let treasury = parse_address("treasury", &cfg.treasury)?;
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

/// Sets up redacted logging and funds the accounts. A failure is redacted before eyre
/// prints it, since a transport error quotes the node URL, which may hold a key.
#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenvy::dotenv().ok();
  redact::register_env();
  let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
  tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_writer(|| Redacting(std::io::stderr()))
    .init();
  fund_accounts()
    .await
    .map_err(|e| eyre::eyre!(redact::redact(&format!("{e:#}"))))
}
