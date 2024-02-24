mod arbitrage;
mod eth_wallet;
mod uniswap;

use crate::eth_wallet::EthWallet;
use dotenv::dotenv;
use ethers::{
  middleware::SignerMiddleware,
  providers::{Http, Provider},
  signers::Wallet,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenv().ok();
  println!("Starting the bot...");
  let rpc_url = std::env::var("RPC_URL").unwrap();
  let wallet = Wallet::try_from(std::env::var("PRIVATE_KEY").unwrap().to_string())?;
  let provider: Provider<ethers::providers::Http> = Provider::<Http>::try_from(rpc_url)?;
  let client = SignerMiddleware::new(provider, wallet);

  // Wallet Initialization
  let arb_wallet = EthWallet::new(
    client,
    "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse()?,
    "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f".parse()?,
    "0x45053C90301E8A2A6A1DF1F0791B553496Dd6c7f".parse()?,
  );

  let pair = arb_wallet
    .get_pair(
      "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse()?,
      "0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE".parse()?,
    )
    .await?;
  println!("Reserves: {:?}", pair.clone().get_reserves().call().await?);
  let spender = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse()?;
  let value = "1000000000000000".parse()?;
  let binding = pair.approve(spender, value);
  let res = binding.send().await?.await?;
  println!("{:?}\nDone...", res);
  Ok(())
}
