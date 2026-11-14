// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {QuoteLens} from "lens/QuoteLens.sol";

contract QuoteLensTest is Test {
    address internal constant POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;
    uint256 internal constant RESERVE = 1_000_000;
    uint256 internal constant FEE = 3000;
    uint256 internal constant AMOUNT = 1000;

    QuoteLens internal lens;
    FeeToken internal plainIn;
    FeeToken internal plainOut;
    FeeToken internal taxIn;
    FeeToken internal taxOut;

    function setUp() public {
        lens = new QuoteLens();
        plainIn = new FeeToken(0);
        plainOut = new FeeToken(0);
        taxIn = new FeeToken(1000);
        taxOut = new FeeToken(1000);
    }

    function testRuntimeMatchesTheCheckedInBytecode() public view {
        bytes memory checked = vm.parseBytes(string.concat("0x", vm.readFile("contracts/lens/QuoteLens.bin")));
        assertEq(type(QuoteLens).runtimeCode, checked);
    }

    function testUntaxedV2MatchesThePairFormula() public {
        MockPair pair = _pair(plainIn, plainOut);
        uint256 got = lens.quoteV2(address(pair), address(plainIn), address(plainOut), AMOUNT, FEE);
        assertEq(got, _product(AMOUNT, RESERVE, RESERVE, FEE));
    }

    function testInputTaxUsesTheAmountThePairReceived() public {
        MockPair pair = _pair(taxIn, plainOut);
        uint256 got = lens.quoteV2(address(pair), address(taxIn), address(plainOut), AMOUNT, FEE);
        uint256 received = AMOUNT - (AMOUNT * 1000 / 10_000);
        uint256 taxed = _product(received, RESERVE, RESERVE, FEE);
        assertEq(received, 900);
        assertEq(got, taxed);
        assertLt(got, _product(AMOUNT, RESERVE, RESERVE, FEE));
        assertEq(taxIn.balanceOf(address(pair)), RESERVE + received);
    }

    function testOutputTaxReducesTheBalanceDelta() public {
        MockPair pair = _pair(plainIn, taxOut);
        uint256 got = lens.quoteV2(address(pair), address(plainIn), address(taxOut), AMOUNT, FEE);
        uint256 gross = _product(AMOUNT, RESERVE, RESERVE, FEE);
        uint256 tax = gross * 1000 / 10_000;
        assertEq(got, gross - tax);
        assertLt(got, gross);
    }

    function testV3CallbackPaysThePoolAndReturnsTheBalanceDelta() public {
        plainIn.mint(address(lens), AMOUNT);
        MockV3 pool = new MockV3(plainIn, plainOut);
        plainOut.mint(address(pool), AMOUNT);
        uint256 got = lens.quoteV3(address(pool), address(plainIn), address(plainOut), true, int256(AMOUNT));
        assertEq(got, AMOUNT / 2);
        assertEq(plainIn.balanceOf(address(pool)), AMOUNT);
    }

    function testV4SettleTakeAndOutputTax() public {
        MockManager manager = new MockManager();
        vm.etch(POOL_MANAGER, address(manager).code);
        plainIn.mint(address(lens), AMOUNT * 3);
        plainOut.mint(POOL_MANAGER, AMOUNT);
        uint256 got = lens.quoteV4(address(plainIn), address(plainOut), 3000, 60, address(0), true, int256(AMOUNT));
        assertEq(got, AMOUNT / 2);
        assertEq(plainIn.balanceOf(POOL_MANAGER), AMOUNT);

        taxOut.mint(POOL_MANAGER, AMOUNT);
        uint256 taxed = lens.quoteV4(address(plainIn), address(taxOut), 3000, 60, address(0), true, int256(AMOUNT));
        uint256 gross = AMOUNT / 2;
        assertEq(taxed, gross - (gross * 1000 / 10_000));
    }

    function testV4HookRevertFailsTheQuote() public {
        MockManager manager = new MockManager();
        vm.etch(POOL_MANAGER, address(manager).code);
        plainIn.mint(address(lens), AMOUNT);
        vm.expectRevert();
        lens.quoteV4(address(plainIn), address(plainOut), 3000, 60, address(1), true, int256(AMOUNT));
    }

    function testCurveBalanceDeltaUsesTheExchangePull() public {
        plainIn.mint(address(lens), AMOUNT * 2);
        MockCurve pool = new MockCurve(plainIn, plainOut);
        plainOut.mint(address(pool), AMOUNT);
        uint256 got = lens.quoteCurve(address(pool), address(plainIn), address(plainOut), 0, 1, AMOUNT, true);
        assertEq(got, AMOUNT / 2);
        uint256 alt = lens.quoteCurve(address(pool), address(plainIn), address(plainOut), 0, 1, AMOUNT, false);
        assertEq(alt, AMOUNT / 2);
    }

    function _pair(FeeToken tokenIn, FeeToken tokenOut) internal returns (MockPair pair) {
        pair = new MockPair(tokenIn, tokenOut);
        tokenIn.mint(address(pair), RESERVE);
        tokenOut.mint(address(pair), RESERVE);
        tokenIn.mint(address(lens), AMOUNT);
    }

    function _product(uint256 amountIn, uint256 reserveIn, uint256 reserveOut, uint256 feePips)
        internal
        pure
        returns (uint256)
    {
        uint256 factor = 1_000_000 - feePips;
        uint256 inFee = amountIn * factor;
        return inFee * reserveOut / (reserveIn * 1_000_000 + inFee);
    }
}

contract FeeToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    uint256 public taxBps;

    constructor(uint256 taxBps_) {
        taxBps = taxBps_;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        return _move(msg.sender, to, amount);
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        require(allowed >= amount, "allowance");
        if (allowed != type(uint256).max) allowance[from][msg.sender] = allowed - amount;
        return _move(from, to, amount);
    }

    function _move(address from, address to, uint256 amount) internal returns (bool) {
        uint256 tax = amount * taxBps / 10_000;
        require(balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount - tax;
        return true;
    }
}

contract MockPair {
    FeeToken public token0;
    FeeToken public token1;

    constructor(FeeToken tokenIn, FeeToken tokenOut) {
        token0 = tokenIn;
        token1 = tokenOut;
    }

    function getReserves() external pure returns (uint112, uint112, uint32) {
        return (1_000_000, 1_000_000, 0);
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        if (amount0Out > 0) require(token0.transfer(to, amount0Out), "out0");
        if (amount1Out > 0) require(token1.transfer(to, amount1Out), "out1");
    }
}

contract MockV3 {
    FeeToken internal input;
    FeeToken internal output;

    constructor(FeeToken tokenIn, FeeToken tokenOut) {
        input = tokenIn;
        output = tokenOut;
    }

    function swap(address recipient, bool, int256 amountSpecified, uint160, bytes calldata)
        external
        returns (int256, int256)
    {
        uint256 owed = uint256(amountSpecified);
        QuoteLens(payable(msg.sender)).uniswapV3SwapCallback(int256(owed), -int256(owed / 2), "");
        require(output.transfer(recipient, owed / 2), "out");
        require(input.balanceOf(address(this)) == owed, "paid");
        return (int256(owed), -int256(owed / 2));
    }
}

contract MockCurve {
    FeeToken internal coin0;
    FeeToken internal coin1;

    constructor(FeeToken a, FeeToken b) {
        coin0 = a;
        coin1 = b;
    }

    function exchange(uint256 i, uint256 j, uint256 dx, uint256) external returns (uint256) {
        return _swap(i, j, dx);
    }

    function exchange(int128 i, int128 j, uint256 dx, uint256) external returns (uint256) {
        return _swap(uint256(uint128(i)), uint256(uint128(j)), dx);
    }

    function _swap(uint256 i, uint256 j, uint256 dx) internal returns (uint256 out) {
        FeeToken input = i == 0 ? coin0 : coin1;
        FeeToken output = j == 0 ? coin0 : coin1;
        require(input.transferFrom(msg.sender, address(this), dx), "pull");
        out = dx / 2;
        require(output.transfer(msg.sender, out), "push");
    }
}

contract MockManager {
    function unlock(bytes calldata data) external returns (bytes memory) {
        return QuoteLens(payable(msg.sender)).unlockCallback(data);
    }

    function swap(IV4View.PoolKey calldata key, IV4View.SwapParams calldata params, bytes calldata)
        external
        pure
        returns (int256)
    {
        require(key.hooks == address(0), "hook");
        int128 owed = int128(-params.amountSpecified);
        int128 credit = -int128(owed / 2);
        if (params.zeroForOne) return (int256(owed) << 128) | int256(uint256(uint128(credit)));
        return (int256(credit) << 128) | int256(uint256(uint128(owed)));
    }

    function sync(address) external {}

    function settle() external payable returns (uint256) {
        return msg.value;
    }

    function take(address currency, address to, uint256 amount) external {
        require(FeeToken(currency).transfer(to, amount), "take");
    }
}

interface IV4View {
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    struct SwapParams {
        bool zeroForOne;
        int256 amountSpecified;
        uint160 sqrtPriceLimitX96;
    }
}
