// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IWETH, SafeTransfer, V2Leg} from "./legs/V2Leg.sol";
import {IPoolManager, V4Leg} from "./legs/V4Leg.sol";

/// @title Flash arbitrage executor
/// @notice Borrows native ETH from the Uniswap V4 PoolManager inside `unlock`,
/// wraps it once, runs a buy leg (WETH to quote) and a sell leg (quote to
/// WETH), repays the loan and keeps the profit as WETH. Nothing is stored
/// per trade; the trade travels in the unlock payload.
contract FlashArbitrage {
  /// @notice Leg family of a Uniswap V2 style pair.
  uint8 internal constant FAMILY_V2 = 0;
  /// @notice Leg family of a Uniswap V4 pool.
  uint8 internal constant FAMILY_V4 = 1;

  /// @notice One swap of the trade.
  struct Leg {
    /// @notice 0 for V2 style pairs, 1 for V4 pools.
    uint8 family;
    /// @notice Family specific encoding: V2 `(address pair, bool zeroForOne, uint16 feeBps)`,
    /// V4 `(PoolKey key, bool zeroForOne)`.
    bytes data;
  }

  /// @notice The caller is not the owner.
  error NotOwner();
  /// @notice The caller is not an operator.
  error NotOperator();
  /// @notice The unlock callback did not come from the PoolManager.
  error NotPoolManager();
  /// @notice `execute` was called with a zero profit floor.
  error ZeroMinProfit();
  /// @notice A leg names a family this executor does not know.
  error UnknownFamily(uint8 family);
  /// @notice The trade grew the WETH balance by `gained`, less than `want`.
  error NoProfit(uint256 gained, uint256 want);

  /// @notice Uniswap V4 PoolManager that lends the ETH and hosts V4 swaps.
  IPoolManager public immutable pm;
  /// @notice Wrapped native token the profit is measured in.
  IWETH public immutable weth;
  /// @notice Deployer, the only account that sweeps and manages operators.
  address public immutable owner;
  /// @notice The only destination of sweeps.
  address public immutable treasury;
  /// @notice Accounts allowed to call `execute`.
  mapping(address => bool) public operators;

  /// @notice Restricts a function to the owner.
  modifier onlyOwner() {
    if (msg.sender != owner) {
      revert NotOwner();
    }
    _;
  }

  /// @param poolManager Uniswap V4 PoolManager.
  /// @param wrappedNative Wrapped native token (WETH).
  /// @param treasury_ Fixed recipient of every sweep.
  /// @param operators_ Initial operators.
  constructor(address poolManager, address wrappedNative, address treasury_, address[] memory operators_) {
    pm = IPoolManager(poolManager);
    weth = IWETH(wrappedNative);
    owner = msg.sender;
    treasury = treasury_;
    for (uint256 i = 0; i < operators_.length; i++) {
      operators[operators_[i]] = true;
    }
  }

  /// @notice Accepts ether only from unwrapping WETH and from PoolManager takes.
  receive() external payable {
    if (msg.sender != address(weth) && msg.sender != address(pm)) {
      revert NotPoolManager();
    }
  }

  /// @notice Runs `leg` on `amountIn` through its family library.
  function runLeg(Leg memory leg, uint256 amountIn) private returns (uint256) {
    if (leg.family == FAMILY_V2) {
      return V2Leg.swap(leg.data, amountIn);
    }
    if (leg.family == FAMILY_V4) {
      return V4Leg.swap(pm, leg.data, amountIn, weth);
    }
    revert UnknownFamily(leg.family);
  }

  /// @notice Borrows `amount` and runs `buy` then `sell`, reverting unless
  /// the executor's WETH grows by at least `minProfit`.
  /// @param buy Leg from WETH to the quote token.
  /// @param sell Leg from the quote token back to WETH.
  /// @param amount WETH borrowed and fed into `buy`.
  /// @param minProfit Smallest acceptable WETH gain, above zero.
  function execute(Leg calldata buy, Leg calldata sell, uint256 amount, uint256 minProfit) external {
    if (!operators[msg.sender]) {
      revert NotOperator();
    }
    if (minProfit == 0) {
      revert ZeroMinProfit();
    }
    pm.unlock(abi.encode(buy, sell, amount, minProfit));
  }

  /// @notice PoolManager callback: borrows native ETH, wraps it, runs both
  /// legs, repays, and checks the gain. The borrowed amount is excluded from
  /// the gain because it is wrapped in and unwrapped out within the call.
  /// @param data `abi.encode(Leg buy, Leg sell, uint256 amount, uint256 minProfit)`.
  /// @return Empty bytes.
  function unlockCallback(bytes calldata data) external returns (bytes memory) {
    if (msg.sender != address(pm)) {
      revert NotPoolManager();
    }
    (Leg memory buy, Leg memory sell, uint256 amount, uint256 minProfit) =
      abi.decode(data, (Leg, Leg, uint256, uint256));
    uint256 before = weth.balanceOf(address(this));
    pm.take(address(0), address(this), amount);
    weth.deposit{value: amount}();
    runLeg(sell, runLeg(buy, amount));
    weth.withdraw(amount);
    pm.settle{value: amount}();
    uint256 afterBal = weth.balanceOf(address(this));
    uint256 gained = afterBal > before ? afterBal - before : 0;
    if (gained < minProfit) {
      revert NoProfit(gained, minProfit);
    }
    return "";
  }

  /// @notice Sends `amount` of `token` held by the executor to the treasury.
  function sweep(address token, uint256 amount) external onlyOwner {
    SafeTransfer.transfer(token, treasury, amount);
  }

  /// @notice Grants or revokes operator rights for `account`.
  function setOperator(address account, bool allowed) external onlyOwner {
    operators[account] = allowed;
  }
}
