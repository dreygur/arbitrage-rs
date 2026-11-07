// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @notice Minimal ERC20 surface the executor and its legs use.
interface IERC20 {
  /// @notice Token balance of `account`.
  function balanceOf(address account) external view returns (uint256);
  /// @notice Moves `amount` from the caller to `to`.
  function transfer(address to, uint256 amount) external returns (bool);
}

/// @notice Wrapped native token (WETH9).
interface IWETH is IERC20 {
  /// @notice Wraps the attached ether one to one.
  function deposit() external payable;
  /// @notice Unwraps `amount` and sends the ether to the caller.
  function withdraw(uint256 amount) external;
}

/// @notice Minimal Uniswap V2 pair surface for pair level swaps.
interface IUniswapV2Pair {
  /// @notice First token of the pair, the lower address.
  function token0() external view returns (address);
  /// @notice Second token of the pair, the higher address.
  function token1() external view returns (address);
  /// @notice Current reserves and the timestamp of their last update.
  function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
  /// @notice Sends the requested outputs to `to` once the input was transferred in.
  function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

/// @title Token transfers that tolerate tokens without a return value
/// @notice USDT and similar tokens return nothing from `transfer`, which a
/// typed call would fail to decode.
library SafeTransfer {
  /// @notice A token transfer returned false or reverted.
  error TransferFailed(address token);

  /// @notice Transfers `amount` of `token` to `to`, reverting on failure.
  function transfer(address token, address to, uint256 amount) internal {
    (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20.transfer, (to, amount)));
    if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
      revert TransferFailed(token);
    }
  }
}

/// @title Uniswap V2 style leg
/// @notice Swaps at the pair level: transfer in, then `swap` out to the
/// executor, with no router and no approvals.
library V2Leg {
  /// @notice Basis points in one whole.
  uint256 internal constant BPS = 10000;

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
