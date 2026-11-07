// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "./Interfaces.sol";

/// @title Token transfers that tolerate tokens without a return value
/// @notice USDT and similar tokens return nothing from `transfer`, which a
/// typed call would fail to decode.
library SafeTransfer {
  /// @notice A token transfer returned false or reverted.
  /// @param token Token whose transfer failed.
  error TransferFailed(address token);

  /// @notice Transfers `amount` of `token` to `to`, reverting on failure.
  /// @param token ERC20 to move.
  /// @param to Recipient.
  /// @param amount Amount to move.
  function transfer(address token, address to, uint256 amount) internal {
    (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20.transfer, (to, amount)));
    if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
      revert TransferFailed(token);
    }
  }
}
