// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice Minimal ERC20 surface the executor and its legs use.
interface IERC20 {
  /// @notice Token balance of `account`.
  /// @param account Holder to query.
  /// @return Balance in the token's smallest unit.
  function balanceOf(address account) external view returns (uint256);
  /// @notice Moves `amount` from the caller to `to`.
  /// @param to Recipient.
  /// @param amount Amount to move.
  /// @return True on success (some tokens return nothing).
  function transfer(address to, uint256 amount) external returns (bool);
}

/// @notice Wrapped native token (WETH9).
interface IWETH is IERC20 {
  /// @notice Wraps the attached ether one to one.
  function deposit() external payable;
  /// @notice Unwraps `amount` and sends the ether to the caller.
  /// @param amount WETH to unwrap.
  function withdraw(uint256 amount) external;
}

/// @notice Minimal Uniswap V2 pair surface for pair level swaps.
interface IUniswapV2Pair {
  /// @notice First token of the pair, the lower address.
  /// @return Token address.
  function token0() external view returns (address);
  /// @notice Second token of the pair, the higher address.
  /// @return Token address.
  function token1() external view returns (address);
  /// @notice Current reserves and the timestamp of their last update.
  /// @return reserve0 Reserve of token0.
  /// @return reserve1 Reserve of token1.
  /// @return blockTimestampLast Timestamp of the last reserve update.
  function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
  /// @notice Sends the requested outputs to `to` once the input was transferred in.
  /// @param amount0Out token0 to send.
  /// @param amount1Out token1 to send.
  /// @param to Recipient of the outputs.
  /// @param data Flash swap callback data, empty for a plain swap.
  function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

/// @notice Identifies a Uniswap V4 pool; currencies are addresses, zero is native ETH.
struct PoolKey {
  /// @notice Lower currency of the pool.
  address currency0;
  /// @notice Higher currency of the pool.
  address currency1;
  /// @notice LP fee in hundredths of a basis point.
  uint24 fee;
  /// @notice Tick spacing of the pool.
  int24 tickSpacing;
  /// @notice Hooks contract, zero for hookless pools.
  address hooks;
}

/// @notice Parameters of a PoolManager swap.
struct SwapParams {
  /// @notice True to sell currency0 for currency1.
  bool zeroForOne;
  /// @notice Negative for exact input, positive for exact output.
  int256 amountSpecified;
  /// @notice Price the swap may not cross.
  uint160 sqrtPriceLimitX96;
}

/// @notice Minimal Uniswap V4 PoolManager surface for flash borrowing and swaps.
interface IPoolManager {
  /// @notice Unlocks the manager and calls back `unlockCallback(data)` on the caller.
  /// @param data Payload handed to the callback.
  /// @return The callback's return data.
  function unlock(bytes calldata data) external returns (bytes memory);
  /// @notice Sends `amount` of `currency` to `to`, adding a debt to the caller.
  /// @param currency Currency to send, zero for native ETH.
  /// @param to Recipient.
  /// @param amount Amount to send.
  function take(address currency, address to, uint256 amount) external;
  /// @notice Credits the caller with what was paid since the last `sync`, or with `msg.value` for native.
  /// @return paid Amount credited.
  function settle() external payable returns (uint256 paid);
  /// @notice Records the manager's balance of `currency` before an ERC20 payment.
  /// @param currency ERC20 about to be paid.
  function sync(address currency) external;
  /// @notice Swaps in `key`.
  /// @param key Pool to swap in.
  /// @param params Direction, amount and price limit.
  /// @param hookData Data for the pool's hooks, empty for hookless pools.
  /// @return delta BalanceDelta from the caller's point of view, amount0 in the high
  /// 128 bits and amount1 in the low 128 bits, negative meaning owed.
  function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData) external returns (int256 delta);
}
