use ethers::{
  middleware::{SignerMiddleware, Middleware},
  signers::Signer,
  abi::Address,
};
use crate::arbitrage::Arbitrage;
use crate::uniswap::{IUniswapV2Pair, Uniswap};

#[derive(Debug,Clone)]
pub struct EthWallet<M: Middleware + 'static, S: Signer + 'static> {
  pub provider: SignerMiddleware<M, S>,
  pub uniswap: Uniswap<M,S>,
  pub arbitrage: Arbitrage<M,S>,
  pub arbitrage_address: Address,
}

impl<M: Middleware + Clone + 'static, S: Signer + Clone + 'static> EthWallet<M, S> {
  pub fn new(
    provider: SignerMiddleware<M, S>,
    router: Address,
    factory: Address,
    arbitrage_address: Address
  ) -> Self {
    Self {
      provider: provider.clone(),
      uniswap: Uniswap::new(provider.clone(), router, factory),
      arbitrage: Arbitrage::new(provider.clone(), arbitrage_address.clone()),
      arbitrage_address,
    }
  }

  pub async fn get_pair(&self, quote: Address, base: Address) -> eyre::Result<IUniswapV2Pair<SignerMiddleware<M, S>>> {
    let pair = self.uniswap.pair(quote, base).await?;
    Ok(pair)
  }
}