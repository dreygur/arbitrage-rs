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
  "[
    function getPair(address tokenA, address tokenB) public view returns (address pair)
    event PairCreated(address indexed token0, address indexed token1, address pair, uint)
  ]"
);

// Uniswap V2 Pair
abigen!(
  IUniswapV2Pair,
  "[
    event Mint(address indexed sender, uint amount0, uint amount1)
    event Approval(address indexed owner, address indexed spender, uint value)
    event Transfer(address indexed from, address indexed to, uint value)
    event Burn(address indexed sender, uint amount0, uint amount1, address indexed to)
    event Swap(address indexed sender,uint amount0In,uint amount1In,uint amount0Out,uint amount1Out,address indexed to)
    event Sync(uint112 reserve0, uint112 reserve1)
    function name() external pure returns (string memory)
    function symbol() external pure returns (string memory)
    function decimals() external pure returns (uint8)
    function totalSupply() external view returns (uint)
    function balanceOf(address owner) external view returns (uint)
    function allowance(address owner, address spender) external view returns (uint)
    function approve(address spender, uint value) external returns (bool)
    function transfer(address to, uint value) external returns (bool)
    function transferFrom(address from, address to, uint value) external returns (bool)
    function DOMAIN_SEPARATOR() external view returns (bytes32)
    function PERMIT_TYPEHASH() external pure returns (bytes32)
    function nonces(address owner) external view returns (uint)
    function permit(address owner, address spender, uint value, uint deadline, uint8 v, bytes32 r, bytes32 s) external
    function MINIMUM_LIQUIDITY() external pure returns (uint)
    function factory() external view returns (address)
    function token0() external view returns (address)
    function token1() external view returns (address)
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
    function price0CumulativeLast() external view returns (uint)
    function price1CumulativeLast() external view returns (uint)
    function kLast() external view returns (uint)
    function mint(address to) external returns (uint liquidity)
    function burn(address to) external returns (uint amount0, uint amount1)
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external
    function skim(address to) external
    function sync() external
    function initialize(address, address) external
  ]"
);

#[derive(Debug,Clone)]
pub struct Uniswap<M: Middleware + 'static, S: Signer + 'static> {
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

  pub fn router(&self) -> eyre::Result<IUniswapV2Router<SignerMiddleware<M, S>>> {
    Ok(IUniswapV2Router::new(self.router, self.provider.clone()))
  }

  pub fn factory(&self) -> eyre::Result<IUniswapV2Factory<SignerMiddleware<M, S>>> {
    Ok(IUniswapV2Factory::new(self.factory, self.provider.clone()))
  }

  pub async fn pair(&self, quote: Address, base: Address) -> eyre::Result<IUniswapV2Pair<SignerMiddleware<M, S>>> {
    let f = self.factory()?;
    let p = f.get_pair(quote, base).call().await?;
    Ok(IUniswapV2Pair::new(p, self.provider.clone()))
  }
}

