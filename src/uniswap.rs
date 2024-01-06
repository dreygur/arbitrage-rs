use ethers::{
  prelude::abigen,
  providers::{Http, Provider},
  types::Address,
};
use std::sync::Arc;

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

pub fn router_v2(provider: Provider<Http>) -> eyre::Result<IUniswapV2Router<Provider<Http>>> {
  // Initialize a new instance of the Weth/Dai Uniswap V2 pair contract
  let p = Arc::new(provider.clone());
  let pair_address: Address = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse()?;
  Ok(IUniswapV2Router::new(pair_address, p))
}
