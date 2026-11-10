// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IUniswapV2Pair} from "../Interfaces.sol";
import {SafeTransfer} from "../SafeTransfer.sol";

/// @title Uniswap V2 style leg
/// @notice Swaps at the pair level: transfer in, then `swap` out to the
/// executor, with no router and no approvals.
library V2Leg {
  /// @notice Basis points in one whole.
  uint256 internal constant BPS = 10000;

  /// @notice Input and output tokens of the leg in `data`.
  /// @param data `abi.encode(address pair, bool zeroForOne, uint16 feeBps)`.
  /// @return tokenIn Token the leg spends.
  /// @return tokenOut Token the leg returns.
  function currencies(bytes memory data) internal view returns (address tokenIn, address tokenOut) {
    (address pair, bool zeroForOne,) = abi.decode(data, (address, bool, uint16));
    IUniswapV2Pair p = IUniswapV2Pair(pair);
    (tokenIn, tokenOut) = zeroForOne ? (p.token0(), p.token1()) : (p.token1(), p.token0());
  }

  /// @notice Swaps `amountIn` through the pair in `data` and returns the output.
  /// @param data `abi.encode(address pair, bool zeroForOne, uint16 feeBps)`.
  /// @param amountIn Input amount, already held by the executor.
  /// @return amountOut Output amount, now held by the executor.
  function swap(bytes memory data, uint256 amountIn) internal returns (uint256 amountOut) {
    (address pair, bool zeroForOne, uint16 feeBps) = abi.decode(data, (address, bool, uint16));
    IUniswapV2Pair p = IUniswapV2Pair(pair);
    (uint112 r0, uint112 r1,) = p.getReserves();
    (uint256 rIn, uint256 rOut) = zeroForOne ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
    uint256 inAfterFee = amountIn * (BPS - feeBps);
    amountOut = inAfterFee * rOut / (rIn * BPS + inAfterFee);
    SafeTransfer.transfer(zeroForOne ? p.token0() : p.token1(), pair, amountIn);
    if (zeroForOne) {
      p.swap(0, amountOut, address(this), "");
    } else {
      p.swap(amountOut, 0, address(this), "");
    }
  }
}
