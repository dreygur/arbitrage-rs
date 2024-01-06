mod uniswap;
use ethers::{middleware::{SignerMiddleware, Middleware}, providers::Provider, signers::{Wallet,Signer}};

use ethers::{prelude::abigen, types::Address};
use std::sync::Arc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
  let rpc_url = "http://127.0.0.1:8545";
  let wallet = Wallet::try_from(
    "0xe30749948a5e0bc4f6c01bc2b31330ddf67dacd09c25d488b9e45e51b1c117d1".to_string(),
  )?;
  let provider: Provider<ethers::providers::Http> = Provider::try_from(rpc_url)?;
  let client = SignerMiddleware::new(provider, wallet);

  let a = uniswap::router_v2(client.clone());
  // Use the get_reserves() function to fetch the pool reserves
  let c = a.expect("REASON")
    .get_amounts_out(
      "1000000000000000".parse()?,
      vec![
        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse()?,
        "0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE".parse()?,
      ],
    )
    .call()
    .await?;
  println!("{:?}", c);

  Ok(())
}
