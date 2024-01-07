mod uniswap;

use ethers::{middleware::{SignerMiddleware}, providers::{Provider, Http}, signers::{Wallet}};

#[tokio::main]
async fn main() -> eyre::Result<()> {
  println!("Starting the bot...");
  let rpc_url = "http://127.0.0.1:8545";
  let wallet = Wallet::try_from(
    "0xe30749948a5e0bc4f6c01bc2b31330ddf67dacd09c25d488b9e45e51b1c117d1".to_string(),
  )?;
  let provider: Provider<ethers::providers::Http> = Provider::<Http>::try_from(rpc_url)?;
  let client = SignerMiddleware::new(provider, wallet);

  let u = uniswap::Uniswap::new(client, "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse()?, "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f".parse()?);
  let pair = u.pair("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse()?,"0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE".parse()?).await?;
  // println!("{:?}", pair.clone().get_reserves().call().await?);
  let spender = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse()?;
  let value = "1000000000000000".parse()?;
  let binding = pair.approve(spender, value);
   let res = binding.send().await?.await?;
  println!("{:?}\nDone...", res);

  // Use the get_reserves() function to fetch the pool reserves
  // let a = u.router_v2();
  // let c = a.expect("REASON")
  //   .get_amounts_out(
  //     "1000000000000000".parse()?,
  //     vec![
  //       "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse()?,
  //       "0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE".parse()?,
  //     ],
  //   )
  //   .call()
  //   .await?;
  // println!("{:?}", c);

  Ok(())
}
