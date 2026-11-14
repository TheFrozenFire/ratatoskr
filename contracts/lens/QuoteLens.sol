// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

/// Simulates an exact-input swap and returns the output-token balance delta.
///
/// Vendor quoters revert inside the callback before the token `transfer` runs,
/// so a taxed token or a v4 hook quotes a better price than execution pays.
/// This contract performs the swap. The output is the recipient's balance
/// change, which includes an output tax. An input tax is whatever the pool
/// actually received. The client installs this runtime with a state override
/// and does not deploy it.
contract QuoteLens {
    address internal constant POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;
    uint256 internal constant DENOM = 1_000_000;
    uint256 internal constant SLOT_POOL = 0x510e5101;
    uint256 internal constant SLOT_TOKEN = 0x510e5102;
    uint256 internal constant SLOT_OUT = 0x510e5103;
    uint160 internal constant MIN_SQRT = 4295128740;
    uint160 internal constant MAX_SQRT = 1461446703485210103287273052203988822378723970341;

    receive() external payable {}

    function quoteV2(address pair, address tokenIn, address tokenOut, uint256 amountIn, uint256 feePips)
        external
        returns (uint256)
    {
        uint256 before = balanceOf(tokenOut, address(this));
        safeTransfer(tokenIn, pair, amountIn);
        uint256 out = v2Out(pair, tokenIn, tokenOut, feePips);
        if (out == 0) return 0;
        address token0 = IV2Pair(pair).token0();
        if (tokenOut == token0) IV2Pair(pair).swap(out, 0, address(this), "");
        else IV2Pair(pair).swap(0, out, address(this), "");
        return balanceOf(tokenOut, address(this)) - before;
    }

    function quoteV3(address pool, address tokenIn, address tokenOut, bool zeroForOne, int256 amountIn)
        external
        returns (uint256)
    {
        require(amountIn > 0, "amount");
        arm(pool, tokenIn);
        uint256 before = balanceOf(tokenOut, address(this));
        IV3Pool(pool).swap(address(this), zeroForOne, amountIn, zeroForOne ? MIN_SQRT : MAX_SQRT, "");
        return balanceOf(tokenOut, address(this)) - before;
    }

    function quoteV4(
        address currency0,
        address currency1,
        uint24 fee,
        int24 tickSpacing,
        address hooks,
        bool zeroForOne,
        int256 amountIn
    ) external returns (uint256) {
        require(amountIn > 0, "amount");
        bytes memory data = abi.encode(currency0, currency1, fee, tickSpacing, hooks, zeroForOne, amountIn);
        IV4Manager(POOL_MANAGER).unlock(data);
        return tLoad(SLOT_OUT);
    }

    function quoteMetric(
        address pool,
        address tokenIn,
        address tokenOut,
        bool zeroForOne,
        int128 amountIn,
        uint128 limit
    ) external returns (uint256) {
        require(amountIn > 0, "amount");
        arm(pool, tokenIn);
        uint256 before = balanceOf(tokenOut, address(this));
        IMetricPool(pool).swap(address(this), zeroForOne, amountIn, limit, "", "");
        return balanceOf(tokenOut, address(this)) - before;
    }

    function quoteCurve(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 i,
        uint256 j,
        uint256 amountIn,
        bool uintIndex
    ) external returns (uint256) {
        require(tokenIn != address(0) && tokenOut != address(0), "native curve");
        safeApprove(tokenIn, pool, amountIn);
        uint256 before = balanceOf(tokenOut, address(this));
        if (uintIndex) ICurve256(pool).exchange(i, j, amountIn, 0);
        else ICurve128(pool).exchange(toInt128(i), toInt128(j), amountIn, 0);
        return balanceOf(tokenOut, address(this)) - before;
    }

    function quoteBalancerV2(address vault, bytes32 poolId, address tokenIn, address tokenOut, uint256 amountIn)
        external
        returns (uint256)
    {
        safeApprove(tokenIn, vault, amountIn);
        uint256 before = balanceOf(tokenOut, address(this));
        IBal2(vault).swap(bal2Swap(poolId, tokenIn, tokenOut, amountIn), bal2Funds(), 0, type(uint256).max);
        return balanceOf(tokenOut, address(this)) - before;
    }

    function quoteBalancerV3(address vault, address pool, address tokenIn, address tokenOut, uint256 amountIn)
        external
        returns (uint256)
    {
        bytes memory data = abi.encodeCall(this.bal3Swap, (vault, pool, tokenIn, tokenOut, amountIn));
        IBal3(vault).unlock(data);
        return tLoad(SLOT_OUT);
    }

    /// The v3 vault raw-calls `msg.sender` with this calldata.
    function bal3Swap(address vault, address pool, address tokenIn, address tokenOut, uint256 amountIn)
        external
        returns (bytes memory)
    {
        require(msg.sender == vault, "vault");
        (, uint256 inRaw, uint256 outRaw) = IBal3(vault)
            .swap(
                IBal3.VaultSwapParams({
                    kind: IBal3.SwapKind.EXACT_IN,
                    pool: pool,
                    tokenIn: tokenIn,
                    tokenOut: tokenOut,
                    amountGivenRaw: amountIn,
                    limitRaw: 0,
                    userData: ""
                })
            );
        safeTransfer(tokenIn, vault, inRaw);
        IBal3(vault).settle(tokenIn, inRaw);
        uint256 before = balanceOf(tokenOut, address(this));
        IBal3(vault).sendTo(tokenOut, address(this), outRaw);
        tStore(SLOT_OUT, balanceOf(tokenOut, address(this)) - before);
        return "";
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        require(msg.sender == POOL_MANAGER, "manager");
        tStore(SLOT_OUT, settleV4(data));
        return "";
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        payOwed(amount0Delta, amount1Delta);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        payOwed(amount0Delta, amount1Delta);
    }

    function algebraSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        payOwed(amount0Delta, amount1Delta);
    }

    function metricOmmSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        payOwed(amount0Delta, amount1Delta);
    }

    function v2Out(address pair, address tokenIn, address tokenOut, uint256 feePips) internal view returns (uint256) {
        require(feePips <= DENOM, "fee");
        (uint112 reserve0, uint112 reserve1,) = IV2Pair(pair).getReserves();
        address token0 = IV2Pair(pair).token0();
        uint256 reserveIn;
        uint256 reserveOut;
        if (tokenIn == token0) {
            require(tokenOut == IV2Pair(pair).token1(), "token");
            reserveIn = reserve0;
            reserveOut = reserve1;
        } else {
            require(tokenIn == IV2Pair(pair).token1() && tokenOut == token0, "token");
            reserveIn = reserve1;
            reserveOut = reserve0;
        }
        uint256 bal = balanceOf(tokenIn, pair);
        require(bal >= reserveIn, "reserve");
        uint256 actualIn = bal - reserveIn;
        if (actualIn == 0 || reserveOut == 0) return 0;
        uint256 factor = DENOM - feePips;
        uint256 inFee = actualIn * factor;
        return inFee * reserveOut / (reserveIn * DENOM + inFee);
    }

    function settleV4(bytes calldata data) internal returns (uint256) {
        (address currency0, address currency1, uint24 fee, int24 spacing, address hooks, bool zeroForOne, int256 amountIn)
            = abi.decode(data, (address, address, uint24, int24, address, bool, int256));
        int256 delta = IV4Manager(POOL_MANAGER)
            .swap(
                IV4Manager.PoolKey(currency0, currency1, fee, spacing, hooks),
                IV4Manager.SwapParams(zeroForOne, -amountIn, zeroForOne ? MIN_SQRT : MAX_SQRT),
                ""
            );
        (int128 amount0, int128 amount1) = unpack(delta);
        settlePositive(currency0, amount0);
        settlePositive(currency1, amount1);
        uint256 got0 = takeNegative(currency0, amount0);
        uint256 got1 = takeNegative(currency1, amount1);
        return zeroForOne ? got1 : got0;
    }

    function settlePositive(address currency, int128 delta) internal {
        if (delta <= 0) return;
        uint256 amount = uint256(uint128(delta));
        if (currency == address(0)) {
            IV4Manager(POOL_MANAGER).settle{value: amount}();
            return;
        }
        IV4Manager(POOL_MANAGER).sync(currency);
        safeTransfer(currency, POOL_MANAGER, amount);
        IV4Manager(POOL_MANAGER).settle();
    }

    function takeNegative(address currency, int128 delta) internal returns (uint256) {
        if (delta >= 0) return 0;
        uint256 amount = uint256(uint128(-delta));
        uint256 before = balanceOf(currency, address(this));
        IV4Manager(POOL_MANAGER).take(currency, address(this), amount);
        return balanceOf(currency, address(this)) - before;
    }

    function payOwed(int256 amount0Delta, int256 amount1Delta) internal {
        require(msg.sender == address(uint160(tLoad(SLOT_POOL))), "callback");
        require(amount0Delta <= 0 || amount1Delta <= 0, "both");
        uint256 owed;
        if (amount0Delta > 0) owed = uint256(amount0Delta);
        else if (amount1Delta > 0) owed = uint256(amount1Delta);
        else return;
        safeTransfer(address(uint160(tLoad(SLOT_TOKEN))), msg.sender, owed);
    }

    function arm(address pool, address tokenIn) internal {
        tStore(SLOT_POOL, uint256(uint160(pool)));
        tStore(SLOT_TOKEN, uint256(uint160(tokenIn)));
    }

    function bal2Swap(bytes32 poolId, address tokenIn, address tokenOut, uint256 amountIn)
        internal
        pure
        returns (IBal2.SingleSwap memory)
    {
        return IBal2.SingleSwap({
            poolId: poolId,
            kind: IBal2.SwapKind.GIVEN_IN,
            assetIn: tokenIn,
            assetOut: tokenOut,
            amount: amountIn,
            userData: ""
        });
    }

    function bal2Funds() internal view returns (IBal2.FundManagement memory) {
        return IBal2.FundManagement({
            sender: address(this),
            fromInternalBalance: false,
            recipient: payable(address(this)),
            toInternalBalance: false
        });
    }

    function unpack(int256 delta) internal pure returns (int128 amount0, int128 amount1) {
        amount0 = int128(delta >> 128);
        amount1 = int128(delta);
    }

    function toInt128(uint256 value) internal pure returns (int128) {
        require(value <= uint256(uint128(type(int128).max)), "index");
        return int128(int256(value));
    }

    function safeTransfer(address token, address to, uint256 amount) internal {
        (bool ok, bytes memory data) = token.call(abi.encodeWithSelector(0xa9059cbb, to, amount));
        require(ok && (data.length == 0 || (data.length >= 32 && abi.decode(data, (bool)))), "transfer");
    }

    function safeApprove(address token, address spender, uint256 amount) internal {
        if (allowanceOf(token, spender) != 0) callApprove(token, spender, 0);
        callApprove(token, spender, amount);
    }

    function callApprove(address token, address spender, uint256 amount) internal {
        (bool ok, bytes memory data) = token.call(abi.encodeWithSelector(0x095ea7b3, spender, amount));
        require(ok && (data.length == 0 || (data.length >= 32 && abi.decode(data, (bool)))), "approve");
    }

    function allowanceOf(address token, address spender) internal view returns (uint256) {
        (bool ok, bytes memory data) =
            token.staticcall(abi.encodeWithSelector(0xdd62ed3e, address(this), spender));
        if (!ok || data.length < 32) return 0;
        return abi.decode(data, (uint256));
    }

    function balanceOf(address token, address holder) internal view returns (uint256) {
        if (token == address(0)) return holder == address(this) ? address(this).balance : 0;
        (bool ok, bytes memory data) = token.staticcall(abi.encodeWithSelector(0x70a08231, holder));
        if (!ok || data.length < 32) return 0;
        return abi.decode(data, (uint256));
    }

    function tStore(uint256 slot, uint256 value) internal {
        assembly {
            tstore(slot, value)
        }
    }

    function tLoad(uint256 slot) internal view returns (uint256 value) {
        assembly {
            value := tload(slot)
        }
    }
}

interface IV2Pair {
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IV3Pool {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external
        returns (int256, int256);
}

interface IV4Manager {
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

    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey calldata key, SwapParams calldata params, bytes calldata hookData) external returns (int256);
    function sync(address currency) external;
    function settle() external payable returns (uint256);
    function take(address currency, address to, uint256 amount) external;
}

interface IMetricPool {
    function swap(
        address recipient,
        bool zeroForOne,
        int128 amountSpecified,
        uint128 priceLimitX64,
        bytes calldata callbackData,
        bytes calldata extensionData
    ) external returns (int128, int128);
}

interface ICurve128 {
    function exchange(int128 i, int128 j, uint256 dx, uint256 minDy) external returns (uint256);
}

interface ICurve256 {
    function exchange(uint256 i, uint256 j, uint256 dx, uint256 minDy) external returns (uint256);
}

interface IBal2 {
    enum SwapKind {
        GIVEN_IN,
        GIVEN_OUT
    }

    struct SingleSwap {
        bytes32 poolId;
        SwapKind kind;
        address assetIn;
        address assetOut;
        uint256 amount;
        bytes userData;
    }

    struct FundManagement {
        address sender;
        bool fromInternalBalance;
        address payable recipient;
        bool toInternalBalance;
    }

    function swap(SingleSwap memory singleSwap, FundManagement memory funds, uint256 limit, uint256 deadline)
        external
        payable
        returns (uint256);
}

interface IBal3 {
    enum SwapKind {
        EXACT_IN,
        EXACT_OUT
    }

    struct VaultSwapParams {
        SwapKind kind;
        address pool;
        address tokenIn;
        address tokenOut;
        uint256 amountGivenRaw;
        uint256 limitRaw;
        bytes userData;
    }

    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(VaultSwapParams memory params)
        external
        returns (uint256 amountCalculatedRaw, uint256 amountInRaw, uint256 amountOutRaw);
    function settle(address token, uint256 amountHint) external returns (uint256);
    function sendTo(address token, address to, uint256 amount) external;
}
