//! Sends the profit the executor holds in the base token to the treasury, as the owner.

use std::path::PathBuf;

use alloy::network::EthereumWallet;
use alloy::providers::{Provider, ProviderBuilder};
use arbitrage::chain::evm::accounts::signers;
use arbitrage::chain::evm::bindings::{FlashArbitrage, IERC20};
use arbitrage::chain::evm::config::EvmConfig;
use arbitrage::chain::evm::executor::{current_fees, read_record, setup_gas};
use arbitrage::chain::evm::gas::gas_cost;
use arbitrage::chain::evm::{env_var, parse_address};
use arbitrage::config::NetworkConfig;
use arbitrage::error::Error;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Network swept when `NETWORK` is unset.
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
  let owner = signers(&cfg.accounts)?
    .into_iter()
    .nth(cfg.owner)
    .ok_or_else(|| Error::Config("owner: index is outside the accounts list".into()))?;
  let weth = parse_address("base_token", &cfg.base_token)?;
  let path = PathBuf::from(format!(".arbitrage-{network}.json"));
  let record = read_record(&path)?
    .ok_or_else(|| Error::Config(format!("{}: no executor recorded", path.display())))?;
  if record.owner != owner.address() {
    return Err(Error::Config(format!("{}: owner differs from the config", path.display())).into());
  }
  let provider = ProviderBuilder::new()
    .wallet(EthereumWallet::from(owner))
    .connect(&env_var("RPC_URL")?)
    .await?;
  if provider.get_code_at(record.address).await?.is_empty() {
    return Err(
      Error::Contract(format!(
        "executor in {} has no code; run the bot to deploy",
        path.display()
      ))
      .into(),
    );
  }
  let held = IERC20::new(weth, &provider)
    .balanceOf(record.address)
    .call()
    .await?;
  if held.is_zero() {
    info!(executor = %record.address, "nothing to sweep");
    return Ok(());
  }
  let (cap, tip) = current_fees(&provider, &evm).await?;
  let executor = FlashArbitrage::new(record.address, &provider);
  let call = executor.sweep(weth, held);
  let gas = setup_gas(call.estimate_gas().await?);
  info!(executor = %record.address, amount = %held, gas, fee_cap = cap, worst_case = %gas_cost(gas, cap), "sweeping");
  let receipt = call
    .gas(gas)
    .max_fee_per_gas(cap)
    .max_priority_fee_per_gas(tip)
    .send()
    .await?
    .get_receipt()
    .await?;
  if !receipt.status() {
    return Err(Error::Contract("sweep reverted".into()).into());
  }
  info!(tx = %receipt.transaction_hash, amount = %held, treasury = %record.treasury, "swept");
  Ok(())
}
