//! Solidity interfaces the adapter talks to and the executor artifact.

// `sol!` generates a constructor with one argument per event field, and the V4 `Swap` event
// has eight; the lint fires on macro output this module cannot change.
#![allow(
  clippy::too_many_arguments,
  reason = "sol! output for events with many fields"
)]

use alloy::sol;

sol! {
  /// Uniswap V2 factory, used to find pairs.
  #[sol(rpc)]
  interface IUniswapV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address pair);
    function allPairs(uint256 index) external view returns (address pair);
    function allPairsLength() external view returns (uint256 length);
  }

  /// Uniswap V2 pair, read for reserves and watched for `Sync`.
  #[sol(rpc)]
  interface IUniswapV2Pair {
    event Sync(uint112 reserve0, uint112 reserve1);
    function token0() external view returns (address token);
    function token1() external view returns (address token);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes data) external;
  }

  /// Uniswap V2 router, used only for its pure `getAmountOut` in tests.
  #[sol(rpc)]
  interface IUniswapV2Router02 {
    function getAmountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut) external pure returns (uint256 amountOut);
  }

  /// Uniswap V4 PoolManager events that mark a pool as changed.
  #[sol(rpc)]
  interface IPoolManager {
    event Swap(
      bytes32 indexed id,
      address indexed sender,
      int128 amount0,
      int128 amount1,
      uint160 sqrtPriceX96,
      uint128 liquidity,
      int24 tick,
      uint24 fee
    );
    event ModifyLiquidity(
      bytes32 indexed id,
      address indexed sender,
      int24 tickLower,
      int24 tickUpper,
      int256 liquidityDelta,
      bytes32 salt
    );
  }

  /// Uniswap V4 StateView, the read side of the PoolManager.
  #[sol(rpc)]
  interface IStateView {
    function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
    function getLiquidity(bytes32 poolId) external view returns (uint128 liquidity);
    function getTickBitmap(bytes32 poolId, int16 tick) external view returns (uint256 tickBitmap);
    function getTickLiquidity(bytes32 poolId, int24 tick) external view returns (uint128 liquidityGross, int128 liquidityNet);
  }

  /// Identifies a Uniswap V4 pool.
  struct PoolKey {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
  }

  /// Parameters of a single hop exact input quote.
  struct QuoteExactSingleParams {
    PoolKey poolKey;
    bool zeroForOne;
    uint128 exactAmount;
    bytes hookData;
  }

  /// Uniswap V4 quoter, called with `eth_call` in tests.
  #[sol(rpc)]
  interface IV4Quoter {
    function quoteExactInputSingle(QuoteExactSingleParams params) external returns (uint256 amountOut, uint256 gasEstimate);
  }

  /// One call inside a Multicall3 batch.
  struct Call3 {
    address target;
    bool allowFailure;
    bytes callData;
  }

  /// Outcome of one call inside a Multicall3 batch.
  struct Result3 {
    bool success;
    bytes returnData;
  }

  /// Multicall3, batches reads into one request.
  #[sol(rpc)]
  interface IMulticall3 {
    function aggregate3(Call3[] calls) external payable returns (Result3[] returnData);
    function getEthBalance(address addr) external view returns (uint256 balance);
  }

  /// The slice of ERC20 the adapter uses.
  #[sol(rpc)]
  interface IERC20 {
    event Transfer(address indexed from, address indexed to, uint256 value);
    function balanceOf(address account) external view returns (uint256 balance);
    function transfer(address to, uint256 value) external returns (bool ok);
    function decimals() external view returns (uint8 places);
  }

  /// Wrapped native token: deposit for fork funding, and its wrap and unwrap events, read
  /// with `Transfer` to measure what a trade left in the executor.
  #[sol(rpc)]
  interface IWETH {
    event Deposit(address indexed dst, uint256 wad);
    event Withdrawal(address indexed src, uint256 wad);
    function deposit() external payable;
  }
}

sol!(
  #[sol(rpc)]
  FlashArbitrage,
  "abi/FlashArbitrage.json"
);
