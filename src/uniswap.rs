use ethers::{
  prelude::abigen,
  middleware::{Middleware, SignerMiddleware},
  signers::Signer,
  types::Address,
};
use std::sync::Arc;

// Uniswap V2 Router
abigen!(
  IUniswapV2Router,
  "[
    function swapExactTokensForTokens(uint amountIn,uint amountOutMin,address[] calldata path,address to,uint deadline) external returns (uint[] memory amounts)
    function swapTokensForExactTokens(uint amountOut,uint amountInMax,address[] calldata path,address to,uint deadline) external returns (uint[] memory amounts)
    function getAmountOut(uint amountIn, uint reserveIn, uint reserveOut) external pure returns (uint amountOut)
    function getAmountIn(uint amountOut, uint reserveIn, uint reserveOut) external pure returns (uint amountIn)
    function getAmountsOut(uint amountIn, address[] calldata path) external view returns (uint[] memory amounts)
    function getAmountsIn(uint amountOut, address[] calldata path) external view returns (uint[] memory amounts)
  ]"
);

// Uniswap V2 Factory
abigen!(
  IUniswapV2Factory,
  "[event PairCreated(address indexed token0, address indexed token1, address pair, uint)]"
);

// Uniswap V2 Pair
abigen!(
  IUniswapV2Pair,
  "[
    event Mint(address indexed sender, uint amount0, uint amount1)
    event Swap(address indexed sender,uint amount0In,uint amount1In,uint amount0Out,uint amount1Out,address indexed to)
  ]"
);

#[derive(Debug,Clone)]
pub struct Uniswap<M: Middleware, S: Signer> {
  pub provider: Arc<SignerMiddleware<M, S>>,
  pub router: Address,
  pub factory: Address,
}

impl<M: Middleware, S: Signer> Uniswap<M, S> {
  pub fn new(provider: SignerMiddleware<M, S>, router: Address, factory: Address) -> Self {
    Self {
      provider: Arc::new(provider),
      router,
      factory,
    }
  }

  pub fn router_v2(&self) -> eyre::Result<IUniswapV2Router<SignerMiddleware<M, S>>> {
    Ok(IUniswapV2Router::new(self.router, self.provider.clone()))
  }

  pub fn factory_v2(&self) -> eyre::Result<IUniswapV2Factory<SignerMiddleware<M, S>>> {
    Ok(IUniswapV2Factory::new(self.factory, self.provider.clone()))
  }

  pub fn pair_v2(&self, pair: Address) -> eyre::Result<IUniswapV2Pair<SignerMiddleware<M, S>>> {
    Ok(IUniswapV2Pair::new(pair, self.provider.clone()))
  }
}

