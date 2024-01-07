use std::sync::Arc;
use ethers::{
  prelude::{Signer, SignerMiddleware},
  abi::Address,
  contract::abigen,
  middleware::Middleware
};

// Arbitrage Contract
abigen!(
  IArbitrage,
  "[
    function swapExactTokensForTokens(uint amountIn,uint amountOutMin,address[] calldata path,address to,uint deadline) external returns (uint[] memory amounts)
    function swapTokensForExactTokens(uint amountOut,uint amountInMax,address[] calldata path,address to,uint deadline) external returns (uint[] memory amounts)
    function getAmountOut(uint amountIn, uint reserveIn, uint reserveOut) external pure returns (uint amountOut)
    function getAmountIn(uint amountOut, uint reserveIn, uint reserveOut) external pure returns (uint amountIn)
    function getAmountsOut(uint amountIn, address[] calldata path) external view returns (uint[] memory amounts)
    function getAmountsIn(uint amountOut, address[] calldata path) external view returns (uint[] memory amounts)
  ]"
);

#[derive(Debug,Clone)]
pub struct Arbitrage<M: Middleware + 'static, S: Signer + 'static> {
  pub provider: Arc<SignerMiddleware<M, S>>,
  pub address: Address,
}

impl<M: Middleware, S: Signer> Arbitrage<M, S> {
  pub fn new(provider: SignerMiddleware<M, S>, address: Address) -> Self {
    Self {
      provider: Arc::new(provider),
      address
    }
  }
  pub fn arbitrage(&self) -> eyre::Result<IArbitrage<SignerMiddleware<M, S>>> {
    Ok(IArbitrage::new(self.address, self.provider.clone()))
  }
}