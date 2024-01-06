mod uniswap;
use ethers::providers::Provider;

#[tokio::main]
async fn main() -> eyre::Result<()> {
  let rpc_url = "https://eth.llamarpc.com";
  let provider: Provider<ethers::providers::Http> = Provider::try_from(rpc_url)?;

  let a = uniswap::router_v2(provider)?;
  // Use the get_reserves() function to fetch the pool reserves
  let c = a
    .get_amounts_out(
      "1000000000000000".parse()?,
      vec![
        "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse()?,
        "0x95aD61b0a150d79219dCF64E1E6Cc01f0B64C4cE".parse()?,
      ],
    )
    .call()
    .await?;
  println!("{:?}", c[0]);

  Ok(())
}
