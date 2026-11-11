// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {FlashArbitrage} from "../FlashArbitrage.sol";
import {IERC20, IWETH, IUniswapV2Pair, PoolKey} from "../Interfaces.sol";

/// @notice Uniswap V2 style factory, used to look up pair addresses.
interface IUniswapV2Factory {
  /// @notice Returns the pair of `a` and `b`, or zero when none exists.
  function getPair(address a, address b) external view returns (address);
}

/// @title FlashArbitrage mainnet fork tests
/// @notice Runs the executor against real Uniswap V2, SushiSwap and Uniswap V4
/// pool state; prices are moved only by real swaps, never by mocks.
contract FlashArbitrageTest is Test {
  /// @notice Uniswap V4 PoolManager on mainnet.
  address constant POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;
  /// @notice Wrapped ether on mainnet.
  address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
  /// @notice USDC on mainnet.
  address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
  /// @notice Uniswap V2 factory.
  address constant UNI_V2_FACTORY = 0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f;
  /// @notice SushiSwap factory.
  address constant SUSHI_FACTORY = 0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac;
  /// @notice Treasury stand in (anvil account 9).
  address constant TREASURY = 0xa0Ee7A142d267C1f36714E4a8F75612F20a79720;
  /// @notice Operator (anvil account 1).
  address constant OPERATOR = 0x70997970C51812dc3A010C7d01b50e0d17dc79C8;
  /// @notice Fee of both V2 style pairs, in basis points.
  uint16 constant V2_FEE_BPS = 30;
  /// @notice Leg family of a Uniswap V2 style pair.
  uint8 constant FAMILY_V2 = 0;
  /// @notice Leg family of a Uniswap V4 pool.
  uint8 constant FAMILY_V4 = 1;

  /// @notice Executor under test, deployed by this contract (its owner).
  FlashArbitrage executor;
  /// @notice Uniswap V2 WETH/USDC pair.
  address uniPair;
  /// @notice SushiSwap WETH/USDC pair.
  address sushiPair;

  /// @notice Deploys the executor with one operator and resolves the pairs.
  function setUp() public {
    address[] memory operators = new address[](1);
    operators[0] = OPERATOR;
    executor = new FlashArbitrage(POOL_MANAGER, WETH, TREASURY, operators);
    uniPair = IUniswapV2Factory(UNI_V2_FACTORY).getPair(WETH, USDC);
    sushiPair = IUniswapV2Factory(SUSHI_FACTORY).getPair(WETH, USDC);
  }

  /// @notice Wraps `amount` of fresh ether into WETH held by this contract.
  function mintWeth(uint256 amount) internal {
    vm.deal(address(this), amount);
    IWETH(WETH).deposit{value: amount}();
  }

  /// @notice WETH reserve of a WETH/USDC V2 style pair.
  function wethReserve(address pair) internal view returns (uint256) {
    (uint112 r0, uint112 r1,) = IUniswapV2Pair(pair).getReserves();
    return IUniswapV2Pair(pair).token0() == WETH ? r0 : r1;
  }

  /// @notice Executor WETH, USDC and ether balances. The executor lands on
  /// Foundry's default deploy address, which holds real mainnet tokens, so
  /// tests compare deltas rather than absolute balances.
  function executorBalances() internal view returns (uint256, uint256, uint256) {
    address e = address(executor);
    return (IERC20(WETH).balanceOf(e), IERC20(USDC).balanceOf(e), e.balance);
  }

  /// @notice Sells `amount` WETH into `pair` at the pair level, making WETH
  /// cheap there; the USDC output goes to this contract.
  function dumpWeth(address pair, uint256 amount) internal {
    mintWeth(amount);
    IUniswapV2Pair p = IUniswapV2Pair(pair);
    bool wethIs0 = p.token0() == WETH;
    (uint112 r0, uint112 r1,) = p.getReserves();
    (uint256 rIn, uint256 rOut) = wethIs0 ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
    uint256 out = amount * 997 * rOut / (rIn * 1000 + amount * 997);
    IERC20(WETH).transfer(pair, amount);
    (uint256 out0, uint256 out1) = wethIs0 ? (uint256(0), out) : (out, uint256(0));
    p.swap(out0, out1, address(this), "");
  }

  /// @notice A V2 leg that trades `pair` from WETH to USDC when `wethIn`,
  /// otherwise from USDC to WETH.
  function v2Leg(address pair, bool wethIn) internal view returns (FlashArbitrage.Leg memory) {
    bool wethIs0 = IUniswapV2Pair(pair).token0() == WETH;
    bool zeroForOne = wethIn == wethIs0;
    return FlashArbitrage.Leg(FAMILY_V2, abi.encode(pair, zeroForOne, V2_FEE_BPS));
  }

  /// @notice A V4 leg selling native ETH for USDC on the hookless 0.05% pool.
  function v4EthToUsdcLeg() internal pure returns (FlashArbitrage.Leg memory) {
    PoolKey memory key = PoolKey(address(0), USDC, 500, 10, address(0));
    return FlashArbitrage.Leg(FAMILY_V4, abi.encode(key, true));
  }

  /// @notice Only the PoolManager may enter the unlock callback.
  function test_rejects_foreign_callback() public {
    vm.expectRevert(FlashArbitrage.NotPoolManager.selector);
    executor.unlockCallback("");
  }

  /// @notice Accounts that are not operators cannot execute trades.
  function test_rejects_non_operator() public {
    FlashArbitrage.Leg memory leg = v2Leg(uniPair, true);
    vm.prank(address(0xBEEF));
    vm.expectRevert(FlashArbitrage.NotOperator.selector);
    executor.execute(leg, leg, 1 ether, 1);
  }

  /// @notice A zero profit floor is refused before anything is borrowed.
  function test_rejects_zero_min_profit() public {
    FlashArbitrage.Leg memory leg = v2Leg(uniPair, true);
    vm.prank(OPERATOR);
    vm.expectRevert(FlashArbitrage.ZeroMinProfit.selector);
    executor.execute(leg, leg, 1 ether, 0);
  }

  /// @notice Only the owner sweeps, and the tokens always reach the treasury.
  function test_sweep_pays_treasury_only() public {
    (uint256 executorBefore,,) = executorBalances();
    mintWeth(3 ether);
    IERC20(WETH).transfer(address(executor), 3 ether);

    vm.prank(OPERATOR);
    vm.expectRevert(FlashArbitrage.NotOwner.selector);
    executor.sweep(WETH, 1 ether);

    uint256 treasuryBefore = IERC20(WETH).balanceOf(TREASURY);
    uint256 ownerBefore = IERC20(WETH).balanceOf(address(this));
    executor.sweep(WETH, 2 ether);
    assertEq(IERC20(WETH).balanceOf(TREASURY) - treasuryBefore, 2 ether);
    assertEq(IERC20(WETH).balanceOf(address(executor)) - executorBefore, 1 ether);
    assertEq(IERC20(WETH).balanceOf(address(this)), ownerBefore);
  }

  /// @notice After WETH is dumped on SushiSwap, selling WETH on Uniswap V2 and
  /// buying it back on SushiSwap leaves profit in the executor.
  function test_v2_to_v2_arbitrage_profits() public {
    uint256 sushiWeth = wethReserve(sushiPair);
    dumpWeth(sushiPair, sushiWeth / 10);
    uint256 amount = sushiWeth / 100;
    FlashArbitrage.Leg memory buy = v2Leg(uniPair, true);
    FlashArbitrage.Leg memory sell = v2Leg(sushiPair, false);
    (uint256 wethBefore, uint256 usdcBefore, uint256 ethBefore) = executorBalances();

    vm.prank(OPERATOR);
    executor.execute(buy, sell, amount, 1);

    (uint256 wethAfter, uint256 usdcAfter, uint256 ethAfter) = executorBalances();
    assertGt(wethAfter, wethBefore);
    assertEq(usdcAfter, usdcBefore);
    assertEq(ethAfter, ethBefore);
  }

  /// @notice Buying USDC with native ETH on the V4 pool and selling it on a
  /// dumped SushiSwap pair settles every V4 delta and leaves WETH profit.
  function test_v4_leg_round_trip() public {
    uint256 sushiWeth = wethReserve(sushiPair);
    dumpWeth(sushiPair, sushiWeth / 10);
    uint256 amount = sushiWeth / 100;
    uint256 managerEthBefore = POOL_MANAGER.balance;
    FlashArbitrage.Leg memory sell = v2Leg(sushiPair, false);
    (uint256 wethBefore, uint256 usdcBefore, uint256 ethBefore) = executorBalances();

    vm.prank(OPERATOR);
    executor.execute(v4EthToUsdcLeg(), sell, amount, 1);

    (uint256 wethAfter, uint256 usdcAfter, uint256 ethAfter) = executorBalances();
    assertGt(wethAfter, wethBefore);
    assertEq(usdcAfter, usdcBefore);
    assertEq(ethAfter, ethBefore);
    assertEq(POOL_MANAGER.balance - managerEthBefore, amount);
  }

  /// @notice A round trip through one pair loses its fees, so the trade
  /// reverts with zero gain even when the executor holds earlier profit.
  function test_no_profit_reverts() public {
    mintWeth(5 ether);
    IERC20(WETH).transfer(address(executor), 5 ether);
    FlashArbitrage.Leg memory buy = v2Leg(uniPair, true);
    FlashArbitrage.Leg memory sell = v2Leg(uniPair, false);

    vm.prank(OPERATOR);
    vm.expectRevert(abi.encodeWithSelector(FlashArbitrage.NoProfit.selector, 0, 1));
    executor.execute(buy, sell, 1 ether, 1);
  }

  /// @notice A losing trade on an executor holding no WETH still reports
  /// `NoProfit`, because the gain is checked before the loan is repaid.
  function test_no_profit_reverts_without_prior_weth() public {
    (uint256 wethBefore,,) = executorBalances();
    assertEq(wethBefore, 0);
    FlashArbitrage.Leg memory buy = v2Leg(uniPair, true);
    FlashArbitrage.Leg memory sell = v2Leg(uniPair, false);

    vm.prank(OPERATOR);
    vm.expectRevert(abi.encodeWithSelector(FlashArbitrage.NoProfit.selector, 0, 7));
    executor.execute(buy, sell, 1 ether, 7);
  }

  /// @notice A buy leg that does not spend WETH, or a sell leg that does not
  /// return it, is refused before anything is borrowed.
  function test_rejects_bad_leg_currency() public {
    FlashArbitrage.Leg memory wethToUsdc = v2Leg(uniPair, true);
    FlashArbitrage.Leg memory usdcToWeth = v2Leg(uniPair, false);

    vm.startPrank(OPERATOR);
    vm.expectRevert(FlashArbitrage.BadLegCurrency.selector);
    executor.execute(usdcToWeth, usdcToWeth, 1 ether, 1);
    vm.expectRevert(FlashArbitrage.BadLegCurrency.selector);
    executor.execute(wethToUsdc, wethToUsdc, 1 ether, 1);
    vm.stopPrank();
  }

  /// @notice Only the owner grants or revokes operators, so neither an operator
  /// nor a stranger can add another account that may execute trades.
  function test_set_operator_owner_only() public {
    vm.prank(OPERATOR);
    vm.expectRevert(FlashArbitrage.NotOwner.selector);
    executor.setOperator(address(0xBEEF), true);

    vm.prank(address(0xBEEF));
    vm.expectRevert(FlashArbitrage.NotOwner.selector);
    executor.setOperator(address(0xBEEF), true);
    assertFalse(executor.operators(address(0xBEEF)));

    executor.setOperator(address(0xBEEF), true);
    assertTrue(executor.operators(address(0xBEEF)));
  }

  /// @notice Ether a stranger sends is refused, so no balance can appear
  /// that the profit check would not account for.
  function test_rejects_stranger_ether() public {
    address stranger = address(0xBEEF);
    vm.deal(stranger, 1 ether);
    vm.prank(stranger);
    (bool sent, bytes memory reason) = payable(address(executor)).call{value: 1 ether}("");
    assertFalse(sent);
    assertEq(reason, abi.encodeWithSelector(FlashArbitrage.UnexpectedSender.selector));
    assertEq(stranger.balance, 1 ether);
  }

  /// @notice Ether from anyone but WETH or the PoolManager is refused.
  function test_rejects_unexpected_ether() public {
    vm.deal(address(this), 1 ether);
    vm.expectRevert(FlashArbitrage.UnexpectedSender.selector);
    payable(address(executor)).transfer(1 ether);
  }
}
