// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IWETH, SafeTransfer} from "./V2Leg.sol";

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
  function unlock(bytes calldata data) external returns (bytes memory);
  /// @notice Sends `amount` of `currency` to `to`, adding a debt to the caller.
  function take(address currency, address to, uint256 amount) external;
  /// @notice Credits the caller with what was paid since the last `sync`, or with `msg.value` for native.
  function settle() external payable returns (uint256 paid);
  /// @notice Records the manager's balance of `currency` before an ERC20 payment.
  function sync(address currency) external;
  /// @notice Swaps in `key`; the result is a BalanceDelta from the caller's point of view,
  /// amount0 in the high 128 bits, amount1 in the low 128 bits, negative meaning owed.
  function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData) external returns (int256 delta);
}

/// @title Uniswap V4 leg
/// @notice Swaps exact input inside the executor's unlock and settles both
/// sides at once; the base side enters and leaves as WETH even on native pools.
library V4Leg {
  /// @notice `MIN_SQRT_PRICE + 1`, the limit for zeroForOne swaps.
  uint160 internal constant MIN_PRICE_LIMIT = 4295128740;
  /// @notice `MAX_SQRT_PRICE - 1`, the limit for oneForZero swaps.
  uint160 internal constant MAX_PRICE_LIMIT = 1461446703485210103287273052203988822378723970341;

  /// @notice Pays `amount` of `currency` that the executor owes the manager.
  function pay(IPoolManager pm, address currency, uint256 amount, IWETH weth) private {
    if (currency == address(0)) {
      weth.withdraw(amount);
      pm.settle{value: amount}();
    } else {
      pm.sync(currency);
      SafeTransfer.transfer(currency, address(pm), amount);
      pm.settle();
    }
  }

  /// @notice Takes `amount` of `currency` due to the executor, wrapping native ETH.
  function collect(IPoolManager pm, address currency, uint256 amount, IWETH weth) private {
    pm.take(currency, address(this), amount);
    if (currency == address(0)) {
      weth.deposit{value: amount}();
    }
  }

  /// @notice Swaps `amountIn` through the pool in `data` and returns the output.
  /// @param pm PoolManager, already unlocked by the executor.
  /// @param data `abi.encode(PoolKey key, bool zeroForOne)`.
  /// @param amountIn Exact input, held by the executor (as WETH when native).
  /// @param weth Wrapped native token standing in for native ETH.
  /// @return amountOut Output now held by the executor (as WETH when native).
  function swap(IPoolManager pm, bytes memory data, uint256 amountIn, IWETH weth) internal returns (uint256 amountOut) {
    (PoolKey memory key, bool zeroForOne) = abi.decode(data, (PoolKey, bool));
    // casting to 'int256' is safe because no token amount reaches 2**255
    // forge-lint: disable-next-line(unsafe-typecast)
    int256 exactIn = -int256(amountIn);
    SwapParams memory params = SwapParams(zeroForOne, exactIn, zeroForOne ? MIN_PRICE_LIMIT : MAX_PRICE_LIMIT);
    int256 delta = pm.swap(key, params, "");
    // casting to 'int128' is safe because BalanceDelta packs two int128 halves
    // forge-lint: disable-next-line(unsafe-typecast)
    int128 amount0 = int128(delta >> 128);
    // forge-lint: disable-next-line(unsafe-typecast)
    int128 amount1 = int128(delta);
    (int128 owed, int128 due) = zeroForOne ? (amount0, amount1) : (amount1, amount0);
    (address currencyIn, address currencyOut) =
      zeroForOne ? (key.currency0, key.currency1) : (key.currency1, key.currency0);
    // casting to 'uint256' is safe because exact input leaves owed <= 0 and due >= 0
    // on a hookless pool; a wrong sign becomes a huge amount and the payment reverts
    // forge-lint: disable-next-line(unsafe-typecast)
    pay(pm, currencyIn, uint256(int256(-owed)), weth);
    // forge-lint: disable-next-line(unsafe-typecast)
    amountOut = uint256(int256(due));
    collect(pm, currencyOut, amountOut, weth);
  }
}
