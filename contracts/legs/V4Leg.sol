// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IPoolManager, IWETH, PoolKey, SwapParams} from "../Interfaces.sol";
import {SafeTransfer} from "../SafeTransfer.sol";

/// @title Uniswap V4 leg
/// @notice Swaps exact input inside the executor's unlock and settles both
/// sides at once; the base side enters and leaves as WETH even on native pools.
library V4Leg {
  /// @notice `MIN_SQRT_PRICE + 1`, the limit for zeroForOne swaps.
  uint160 internal constant MIN_PRICE_LIMIT = 4295128740;
  /// @notice `MAX_SQRT_PRICE - 1`, the limit for oneForZero swaps.
  uint160 internal constant MAX_PRICE_LIMIT = 1461446703485210103287273052203988822378723970341;

  /// @notice Pays `amount` of `currency` that the executor owes the manager.
  /// @param pm PoolManager, already unlocked by the executor.
  /// @param currency Currency owed, zero for native ETH (paid by unwrapping WETH).
  /// @param amount Amount owed.
  /// @param weth Wrapped native token standing in for native ETH.
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
  /// @param pm PoolManager, already unlocked by the executor.
  /// @param currency Currency due, zero for native ETH (wrapped into WETH).
  /// @param amount Amount due.
  /// @param weth Wrapped native token standing in for native ETH.
  function collect(IPoolManager pm, address currency, uint256 amount, IWETH weth) private {
    pm.take(currency, address(this), amount);
    if (currency == address(0)) {
      weth.deposit{value: amount}();
    }
  }

  /// @notice Input and output currencies of the leg in `data`, zero meaning native ETH.
  /// @param data `abi.encode(PoolKey key, bool zeroForOne)`.
  /// @return currencyIn Currency the leg spends.
  /// @return currencyOut Currency the leg returns.
  function currencies(bytes memory data) internal pure returns (address currencyIn, address currencyOut) {
    (PoolKey memory key, bool zeroForOne) = abi.decode(data, (PoolKey, bool));
    (currencyIn, currencyOut) = zeroForOne ? (key.currency0, key.currency1) : (key.currency1, key.currency0);
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
