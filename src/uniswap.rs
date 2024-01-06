use ethers::{
  prelude::{abigen, SignerMiddleware},
  providers::{Http, Provider},
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

pub fn router_v2<M, S>(
  provider: SignerMiddleware<Provider<Http>, S>,
) -> eyre::Result<IUniswapV2Router<Provider<Http>>> {
  // Initialize a new instance of the Weth/Dai Uniswap V2 pair contract
  let p = Arc::new(provider);
  let pair_address: Address = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse()?;
  Ok(IUniswapV2Router::new(pair_address, p))
}

// Uniswap V2 Factory
abigen!(
  IUniswapV2Factory,
  "[event PairCreated(address indexed token0, address indexed token1, address pair, uint)]"
);

pub fn factory_v2(provider: Provider<Http>) -> eyre::Result<IUniswapV2Factory<Provider<Http>>> {
  // Initialize a new instance of the Weth/Dai Uniswap V2 pair contract
  let p = Arc::new(provider.clone());
  let factory_address: Address = "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f".parse()?;
  Ok(IUniswapV2Factory::new(factory_address, p))
}

// Uniswap V2 Pair
abigen!(
  IUniswapV2Pair,
  "[
    event Mint(address indexed sender, uint amount0, uint amount1)
    event Swap(address indexed sender,uint amount0In,uint amount1In,uint amount0Out,uint amount1Out,address indexed to)
  ]"
);

pub fn pair_v2(provider: Provider<Http>) -> eyre::Result<IUniswapV2Pair<Provider<Http>>> {
  // Initialize a new instance of the Weth/Dai Uniswap V2 pair contract
  let p = Arc::new(provider.clone());
  let pair_address: Address = "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f".parse()?;
  Ok(IUniswapV2Pair::new(pair_address, p))
}
