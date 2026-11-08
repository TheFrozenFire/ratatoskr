// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

/// One `eth_call` of this creation code reads a batch of pools.
///
/// The call has no `to`. The constructor reverts the payload. Nothing is deployed.
/// Compiled with solc 0.8.28, optimizer on, 200 runs.
///
/// Constructor argument is one `uint256[]`. Each pool is eight words:
///
/// ```
/// kind, id, home, aux, scanFrom, token0, token1, tickSpacing
/// ```
///
/// `scanFrom` is `type(int256).max` to start at the bottom of the tick bitmap.
/// Otherwise it is the next `int16` word, or the next metric bin, from a truncated read.
/// `uint256.max` is not the sentinel: that value is bitmap word −1.
/// `tickSpacing` 0 tells the lens to read `tickSpacing()` on `home`.
///
/// `kind` is the catalog kind. `home` is the pair, the concentrated pool, the
/// v4 PoolManager, the balancer vault, the curve pool, or the metric pool.
/// `aux` is the balancer v2 pool, the balancer v3 vault, or the v4 hooks address.
///
/// Revert payload, 32-byte words:
///
/// ```
/// magic, count, record[count]
/// ```
///
/// `magic` is keccak256("ratatoskr.detail.v1").
/// Each record starts with `nWords`, `kind`, `id`, `flags`, then the body.
/// `flags` bit 0 means the bitmap or bin list was cut. The body carries `nextScan`.
///
/// Concentrated bodies (kinds 5–10) list non-zero bitmap words `(wordPos, bits)`.
/// Tick structs are a second call: `eth_getStorageValues` when the provider has it,
/// otherwise `TickLens`, which batches `ticks()` / `extsload` across pools.
contract DetailLens {
    uint256 internal constant MAGIC =
        0x9a7ad5555fe550f27c28581e7ef0a554b21e5fbb3878333ee61e721f170553aa;
    uint256 internal constant ADDR_MASK = (1 << 160) - 1;
    /// Done, or "start at the bottom". `uint256.max` is int16 word −1, a real bitmap word.
    uint256 internal constant SCAN_ALL = uint256(type(int256).max);
    int256 internal constant MIN_TICK = -887272;
    int256 internal constant MAX_TICK = 887272;
    uint256 internal constant MAX_MAPS = 128;
    uint256 internal constant MAX_BINS = 96;
    uint256 internal constant MAX_COINS = 8;
    uint256 internal constant FLAG_TRUNCATED = 1;

    uint256 internal constant SEL_TOKEN0 = 0x0dfe1681;
    uint256 internal constant SEL_TOKEN1 = 0xd21220a7;
    uint256 internal constant SEL_FEE = 0xddca3f43;
    uint256 internal constant SEL_TICK = 0xd0c93a7c;
    uint256 internal constant SEL_SLOT0 = 0x3850c7bd;
    uint256 internal constant SEL_LIQ = 0x1a686502;
    uint256 internal constant SEL_BITMAP = 0x5339c296;
    uint256 internal constant SEL_RESERVES = 0x0902f1ac;
    uint256 internal constant SEL_SWAP_FEE = 0x54cf2aeb;
    uint256 internal constant SEL_POOL_TOKENS = 0xf94d4668;
    uint256 internal constant SEL_SWAP_FEE_PCT = 0x55c67628;
    uint256 internal constant SEL_WEIGHTS = 0xf89f27ed;
    uint256 internal constant SEL_AMP = 0x6daccffa;
    uint256 internal constant SEL_LIVE_BAL = 0x535cfd8a;
    uint256 internal constant SEL_STATIC_FEE = 0xb45090f9;
    uint256 internal constant SEL_V3_TOKENS = 0xca4f2803;
    uint256 internal constant SEL_COINS = 0xc6610657;
    uint256 internal constant SEL_COINS_I = 0x23746eb8;
    uint256 internal constant SEL_BAL_U = 0x4903b0d1;
    uint256 internal constant SEL_BAL_I = 0x065a80d8;
    uint256 internal constant SEL_A = 0xf446c1d0;
    uint256 internal constant SEL_A_PRECISE = 0x76a2f0f0;
    uint256 internal constant SEL_PRICE_SCALE = 0xa3f7cdd5;
    uint256 internal constant SEL_GAMMA = 0xb1373929;
    uint256 internal constant SEL_D = 0x0f529ba2;
    uint256 internal constant SEL_RATES = 0xfd0684b1;
    uint256 internal constant SEL_MID_FEE = 0x92526c0c;
    uint256 internal constant SEL_OUT_FEE = 0xee8de675;
    uint256 internal constant SEL_FEE_GAMMA = 0x72d4f0e2;
    uint256 internal constant SEL_IMMUTABLES = 0xbcdb4dad;
    uint256 internal constant SEL_QUOTE = 0x171755b1;

    constructor(uint256[] memory items) {
        uint256 pools = items.length / 8;
        if (items.length != pools * 8 || pools > 16) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 base;
        assembly {
            base := mload(0x40)
            if lt(base, 0x800) {
                base := 0x800
            }
        }
        // `base + 32` is the magic and `base + 64` is the count. Records start after both.
        uint256 ptr = base + 96;
        for (uint256 i = 0; i < pools; i++) {
            ptr = writePool(ptr, items, i);
        }
        uint256 total = ptr - base;
        assembly {
            mstore(base, sub(total, 32))
            mstore(add(base, 32), MAGIC)
            mstore(add(base, 64), pools)
            revert(add(base, 32), mload(base))
        }
    }

    function writePool(uint256 ptr, uint256[] memory items, uint256 index) internal view returns (uint256) {
        uint256 at = index * 8;
        uint256 kind = items[at];
        uint256 id = items[at + 1];
        address home = address(uint160(items[at + 2]));
        uint256 start = ptr;
        ptr = put(ptr, 0);
        ptr = put(ptr, kind);
        ptr = put(ptr, id);
        ptr = put(ptr, 0);
        uint256 packed = dispatch(ptr, items, at, kind, id, home);
        // Pointer in the low 64 bits, flags above that. Memory used here stays under 2^64.
        ptr = packed & ((1 << 64) - 1);
        uint256 flags = packed >> 64;
        assembly {
            mstore(start, div(sub(ptr, start), 32))
            mstore(add(start, 96), flags)
        }
        return ptr;
    }

    /// Body writer. Returns `(flags << 64) | ptr`.
    function dispatch(
        uint256 ptr,
        uint256[] memory items,
        uint256 at,
        uint256 kind,
        uint256 id,
        address home
    ) internal view returns (uint256 packed) {
        uint256 flags;
        if (kind >= 1 && kind <= 4) {
            ptr = writeProduct(ptr, home);
        } else if (kind >= 5 && kind <= 9) {
            (ptr, flags) = writeConcentrated(ptr, home, items[at + 4], items[at + 7]);
        } else if (kind == 10) {
            (ptr, flags) = writeV4(
                ptr, home, address(uint160(items[at + 3])), id, items[at + 4], items[at + 5], items[at + 6], items[at + 7]
            );
        } else if (kind == 11) {
            (ptr, flags) = writeBalancerV2(ptr, home, address(uint160(items[at + 3])), id);
        } else if (kind == 12) {
            (ptr, flags) = writeBalancerV3(ptr, home, address(uint160(items[at + 3])));
        } else if (kind == 13) {
            (ptr, flags) = writeCurve(ptr, home);
        } else if (kind == 14) {
            (ptr, flags) = writeMetric(ptr, home, items[at + 4]);
        } else {
            flags = 2;
        }
        packed = (flags << 64) | ptr;
    }

    function writeProduct(uint256 ptr, address pair) internal view returns (uint256) {
        (bool ok0, uint256 t0) = callWord(pair, SEL_TOKEN0, 0, false);
        (bool ok1, uint256 t1) = callWord(pair, SEL_TOKEN1, 0, false);
        (bool okR, uint256 r0, uint256 r1, uint256 ts) = reserves(pair);
        (bool okF, uint256 fee) = callWord(pair, SEL_FEE, 0, false);
        if (!okF || fee > 1_000_000) {
            (okF, fee) = callWord(pair, SEL_SWAP_FEE, 0, false);
            if (!okF || fee > 1_000_000) fee = type(uint256).max;
        }
        ptr = put(ptr, ok0 ? t0 & ADDR_MASK : 0);
        ptr = put(ptr, ok1 ? t1 & ADDR_MASK : 0);
        ptr = put(ptr, okR ? r0 : 0);
        ptr = put(ptr, okR ? r1 : 0);
        ptr = put(ptr, fee);
        ptr = put(ptr, okR ? ts : 0);
        return ptr;
    }

    function writeClHeader(uint256 ptr, address pool, uint256 spacingHint)
        internal
        view
        returns (uint256, int256 spacing)
    {
        ptr = putMasked(ptr, pool, SEL_TOKEN0);
        ptr = putMasked(ptr, pool, SEL_TOKEN1);
        ptr = putWordOr(ptr, pool, SEL_FEE, type(uint256).max);
        spacing = spacingOf(pool, spacingHint);
        ptr = put(ptr, uint256(spacing));
        ptr = putSlot0(ptr, pool);
        ptr = putWordOr(ptr, pool, SEL_LIQ, 0);
        return (ptr, spacing);
    }

    function spacingOf(address pool, uint256 spacingHint) internal view returns (int256 spacing) {
        spacing = int256(spacingHint);
        if (spacing != 0) return spacing;
        (bool okS, uint256 raw) = callWord(pool, SEL_TICK, 0, false);
        if (okS) spacing = sign24(raw);
    }

    function putSlot0(uint256 ptr, address pool) internal view returns (uint256) {
        (bool ok, uint256 sqrt, int256 tick) = readSlot0(pool);
        ptr = put(ptr, ok ? sqrt : 0);
        return put(ptr, uint256(ok ? tick : int256(0)));
    }

    function putMasked(uint256 ptr, address target, uint256 sel) internal view returns (uint256) {
        (bool ok, uint256 word) = callWord(target, sel, 0, false);
        return put(ptr, ok ? word & ADDR_MASK : 0);
    }

    function putWordOr(uint256 ptr, address target, uint256 sel, uint256 fallbackWord)
        internal
        view
        returns (uint256)
    {
        (bool ok, uint256 word) = callWord(target, sel, 0, false);
        return put(ptr, ok ? word : fallbackWord);
    }

    function writeConcentrated(uint256 ptr, address pool, uint256 scanFrom, uint256 spacingHint)
        internal
        view
        returns (uint256, uint256)
    {
        int256 spacing;
        (ptr, spacing) = writeClHeader(ptr, pool, spacingHint);
        uint256 mark = ptr;
        ptr = put(ptr, SCAN_ALL);
        ptr = put(ptr, 0);
        uint256 flags;
        (ptr, flags) = scanView(ptr, mark, pool, spacing, scanFrom);
        return (ptr, flags);
    }

    /// Writes non-zero `(wordPos, bits)` after the reserved `nextScan, count` pair at `mark`.
    function scanView(uint256 ptr, uint256 mark, address pool, int256 spacing, uint256 scanFrom)
        internal
        view
        returns (uint256, uint256 flags)
    {
        uint256 next = SCAN_ALL;
        uint256 maps;
        if (spacing <= 0 || spacing > MAX_TICK) {
            assembly {
                mstore(mark, next)
                mstore(add(mark, 32), 0)
            }
            return (ptr, 2);
        }
        (int256 w, int256 maxW) = wordRange(spacing, scanFrom);
        while (w <= maxW) {
            if (maps == MAX_MAPS || gasleft() < 200_000) {
                flags = FLAG_TRUNCATED;
                next = uint256(w);
                break;
            }
            (bool ok, uint256 bits) = tickBitmap(pool, w);
            if (!ok) {
                flags = 2;
                next = uint256(w);
                break;
            }
            if (bits != 0) {
                ptr = put(ptr, uint256(w));
                ptr = put(ptr, bits);
                maps++;
            }
            w++;
        }
        assembly {
            mstore(mark, next)
            mstore(add(mark, 32), maps)
        }
        return (ptr, flags);
    }

    function writeV4Header(
        uint256 ptr,
        address manager,
        address hooks,
        uint256 id,
        uint256 token0Hint,
        uint256 token1Hint,
        uint256 spacingHint
    ) internal view returns (uint256, uint256 bitmapBase, int256 spacing) {
        uint256 state = v4State(id);
        uint256 slot0Word = extsload(manager, state);
        uint256 liqWord = extsload(manager, state + 3);
        spacing = int256(spacingHint);
        ptr = put(ptr, token0Hint & ADDR_MASK);
        ptr = put(ptr, token1Hint & ADDR_MASK);
        ptr = put(ptr, uint256(uint160(hooks)));
        ptr = put(ptr, (slot0Word >> 208) & ((1 << 24) - 1));
        ptr = put(ptr, uint256(spacing));
        ptr = put(ptr, slot0Word & ((1 << 160) - 1));
        ptr = put(ptr, uint256(sign24(slot0Word >> 160)));
        ptr = put(ptr, liqWord & ((1 << 128) - 1));
        ptr = put(ptr, (slot0Word >> 184) & ((1 << 24) - 1));
        bitmapBase = state + 5;
        return (ptr, bitmapBase, spacing);
    }

    function writeV4(
        uint256 ptr,
        address manager,
        address hooks,
        uint256 id,
        uint256 scanFrom,
        uint256 token0Hint,
        uint256 token1Hint,
        uint256 spacingHint
    ) internal view returns (uint256, uint256) {
        uint256 bitmapBase;
        int256 spacing;
        (ptr, bitmapBase, spacing) = writeV4Header(ptr, manager, hooks, id, token0Hint, token1Hint, spacingHint);
        uint256 mark = ptr;
        ptr = put(ptr, SCAN_ALL);
        ptr = put(ptr, 0);
        uint256 flags;
        (ptr, flags) = scanStorage(ptr, mark, manager, bitmapBase, spacing, scanFrom);
        return (ptr, flags);
    }

    function scanStorage(
        uint256 ptr,
        uint256 mark,
        address manager,
        uint256 bitmapBase,
        int256 spacing,
        uint256 scanFrom
    ) internal view returns (uint256, uint256 flags) {
        uint256 next = SCAN_ALL;
        uint256 maps;
        if (spacing <= 0 || spacing > MAX_TICK) {
            assembly {
                mstore(mark, next)
                mstore(add(mark, 32), 0)
            }
            return (ptr, 2);
        }
        (int256 w, int256 maxW) = wordRange(spacing, scanFrom);
        while (w <= maxW) {
            if (maps == MAX_MAPS || gasleft() < 200_000) {
                flags = FLAG_TRUNCATED;
                next = uint256(w);
                break;
            }
            uint256 bits = bitmapAt(manager, bitmapBase, w);
            if (bits != 0) {
                ptr = put(ptr, uint256(w));
                ptr = put(ptr, bits);
                maps++;
            }
            w++;
        }
        assembly {
            mstore(mark, next)
            mstore(add(mark, 32), maps)
        }
        return (ptr, flags);
    }

    function writeBalancerV2(uint256 ptr, address vault, address pool, uint256 poolId)
        internal
        view
        returns (uint256, uint256)
    {
        (bool okFee, uint256 fee) = callWord(pool, SEL_SWAP_FEE_PCT, 0, false);
        (bool okA, uint256 amp, uint256 precision) = amplification(pool);
        (bool okT, uint256 n) = poolTokens(vault, poolId);
        if (okT) {
            for (uint256 i = 0; i < n * 2; i++) {
                uint256 word = scratchWord(0x80 + i * 32);
                assembly {
                    mstore(add(0x500, mul(i, 32)), word)
                }
            }
        }
        (bool okW, uint256 weightCount) = copyArray(pool, SEL_WEIGHTS, 0, false, 0x80);
        ptr = put(ptr, okFee ? fee : type(uint256).max);
        ptr = put(ptr, n);
        ptr = put(ptr, okA ? amp : 0);
        ptr = put(ptr, okA ? precision : 0);
        ptr = put(ptr, okW ? weightCount : 0);
        if (okT) ptr = copyFrom(ptr, 0x500, n * 2);
        if (okW) ptr = copyScratch(ptr, weightCount);
        return (ptr, okA ? 4 : 0);
    }

    function writeBalancerV3(uint256 ptr, address pool, address vault) internal view returns (uint256, uint256) {
        (bool okFee, uint256 fee) = callWord(vault, SEL_STATIC_FEE, uint256(uint160(pool)), true);
        uint256 nTokens = stashV3Tokens(vault, pool);
        (bool okB, uint256 n) = copyArray(vault, SEL_LIVE_BAL, uint256(uint160(pool)), true, 0x80);
        (bool okW, uint256 weightCount) = copyArray(pool, SEL_WEIGHTS, 0, false, 0x180);
        uint256 count = okB ? n : 0;
        uint256 flags;
        (ptr, flags) = putBalancerHead(ptr, pool, okFee, fee, count, okW, weightCount);
        ptr = putBalancerPairs(ptr, count, nTokens);
        if (okW) ptr = copyFrom(ptr, 0x180, weightCount);
        return (ptr, flags);
    }

    /// `getPoolTokens` compacted at `0x500`. Balances stay at `0x80`. Weights stay at `0x180`.
    function stashV3Tokens(address vault, address pool) internal view returns (uint256 n) {
        (bool ok, uint256 count) = copyArray(vault, SEL_V3_TOKENS, uint256(uint160(pool)), true, 0x500);
        if (ok) n = count;
    }

    function putBalancerHead(
        uint256 ptr,
        address pool,
        bool okFee,
        uint256 fee,
        uint256 n,
        bool okW,
        uint256 weightCount
    ) internal view returns (uint256, uint256) {
        (bool okA, uint256 amp, uint256 precision) = amplification(pool);
        ptr = put(ptr, okFee ? fee : type(uint256).max);
        ptr = put(ptr, n);
        ptr = put(ptr, okA ? amp : 0);
        ptr = put(ptr, okA ? precision : 0);
        ptr = put(ptr, okW ? weightCount : 0);
        return (ptr, okA ? 4 : 0);
    }

    function putBalancerPairs(uint256 ptr, uint256 n, uint256 nTokens) internal pure returns (uint256) {
        for (uint256 i = 0; i < n; i++) {
            uint256 token = i < nTokens ? scratchWord(0x500 + i * 32) & ADDR_MASK : 0;
            ptr = put(ptr, token);
            ptr = put(ptr, scratchWord(0x80 + i * 32));
        }
        return ptr;
    }

    function writeCurve(uint256 ptr, address pool) internal view returns (uint256, uint256) {
        (uint256 coinSel, uint256 balSel, bool coinsIndexed) = curveSelectors(pool);
        uint256 n = coinCount(pool, coinSel);
        uint256 rateCount = stashRates(pool);
        uint256 flags;
        (ptr, flags) = putCurveHead(ptr, pool, n, coinsIndexed);
        for (uint256 c = 0; c < n; c++) {
            ptr = writeCurveCoin(ptr, pool, coinSel, balSel, c, c < rateCount);
            if (scratchWord(ptr - 32) != 0) flags |= 8;
        }
        return (ptr, flags);
    }

    function curveSelectors(address pool)
        internal
        view
        returns (uint256 coinSel, uint256 balSel, bool coinsIndexed)
    {
        (bool ok,) = callWord(pool, SEL_COINS, 0, true);
        if (ok) return (SEL_COINS, SEL_BAL_U, true);
        return (SEL_COINS_I, SEL_BAL_I, false);
    }

    function coinCount(address pool, uint256 coinSel) internal view returns (uint256 n) {
        for (uint256 c = 0; c < MAX_COINS; c++) {
            (bool ok,) = callWord(pool, coinSel, c, true);
            if (!ok) break;
            n = c + 1;
        }
    }

    /// Copies `stored_rates()` into `0x500` and returns how many rates were saved.
    function stashRates(address pool) internal view returns (uint256 n) {
        (bool ok, uint256 count) = copyArray(pool, SEL_RATES, 0, false, 0x80);
        if (!ok) return 0;
        n = count;
        for (uint256 i = 0; i < n; i++) {
            uint256 word = scratchWord(0x80 + i * 32);
            assembly {
                mstore(add(0x500, mul(i, 32)), word)
            }
        }
    }

    function putCurveHead(uint256 ptr, address pool, uint256 n, bool coinsIndexed)
        internal
        view
        returns (uint256, uint256 flags)
    {
        (ptr, flags) = putCurveAmp(ptr, pool, n);
        ptr = putCurveTail(ptr, pool, coinsIndexed);
        return (ptr, flags);
    }

    function putCurveAmp(uint256 ptr, address pool, uint256 n)
        internal
        view
        returns (uint256, uint256 flags)
    {
        (bool okA, uint256 amp) = callWord(pool, SEL_A_PRECISE, 0, false);
        if (okA) {
            // `A_precise` is `A * 100`. The quote uses `A`.
            amp = amp / 100;
        } else {
            (okA, amp) = callWord(pool, SEL_A, 0, false);
        }
        (bool okFee, uint256 fee) = callWord(pool, SEL_FEE, 0, false);
        (bool okG, uint256 gamma) = callWord(pool, SEL_GAMMA, 0, false);
        (bool okD, uint256 d) = callWord(pool, SEL_D, 0, false);
        if (okG && gamma != 0) flags = 8;
        ptr = put(ptr, n);
        ptr = put(ptr, okA ? amp : 0);
        ptr = put(ptr, okFee ? fee : type(uint256).max);
        ptr = put(ptr, okG ? gamma : 0);
        ptr = put(ptr, okD ? d : 0);
        return (ptr, flags);
    }

    function putCurveTail(uint256 ptr, address pool, bool coinsIndexed) internal view returns (uint256) {
        (bool okMid, uint256 mid) = callWord(pool, SEL_MID_FEE, 0, false);
        (bool okOut, uint256 outFee) = callWord(pool, SEL_OUT_FEE, 0, false);
        (bool okFg, uint256 feeGamma) = callWord(pool, SEL_FEE_GAMMA, 0, false);
        ptr = put(ptr, okMid ? mid : 0);
        ptr = put(ptr, okOut ? outFee : 0);
        ptr = put(ptr, okFg ? feeGamma : 0);
        ptr = put(ptr, coinsIndexed ? 1 : 0);
        return ptr;
    }

    function writeCurveCoin(
        uint256 ptr,
        address pool,
        uint256 coinSel,
        uint256 balSel,
        uint256 index,
        bool hasRate
    ) internal view returns (uint256) {
        (, uint256 coin) = callWord(pool, coinSel, index, true);
        (, uint256 bal) = callWord(pool, balSel, index, true);
        (bool okS, uint256 scale) = callWord(pool, SEL_PRICE_SCALE, index, true);
        uint256 rate = hasRate ? scratchWord(0x500 + index * 32) : 0;
        ptr = put(ptr, coin & ADDR_MASK);
        ptr = put(ptr, bal);
        ptr = put(ptr, rate);
        ptr = put(ptr, okS ? scale : 0);
        return ptr;
    }

    function writeMetric(uint256 ptr, address pool, uint256 scanFrom)
        internal
        view
        returns (uint256, uint256)
    {
        int256 lowest;
        int256 highest;
        uint256 flags;
        (ptr, lowest, highest, flags) = writeMetricHead(ptr, pool);
        if (highest < lowest) return (ptr, flags);
        int256 start = lowest;
        if (scanFrom != SCAN_ALL) {
            start = int256(scanFrom);
            if (start < lowest) start = lowest;
        }
        if (start > highest) {
            ptr = put(ptr, SCAN_ALL);
            ptr = put(ptr, 0);
            return (ptr, flags);
        }
        uint256 cut;
        (ptr, cut) = writeMetricBins(ptr, pool, start, highest);
        return (ptr, flags | cut);
    }

    function writeMetricHead(uint256 ptr, address pool)
        internal
        view
        returns (uint256, int256 lowest, int256 highest, uint256 flags)
    {
        uint256 slot0Word = extsload(pool, 0);
        address provider = address(uint160(extsload(pool, 3)));
        bool okImm = callReturn(pool, SEL_IMMUTABLES, 0, false, 0x80, 24 * 32);
        uint256 scale0;
        uint256 scale1;
        highest = -1;
        if (okImm) {
            scale0 = scratchWord(0x80 + 3 * 32);
            scale1 = scratchWord(0x80 + 4 * 32);
            lowest = int256(scratchWord(0x80 + 9 * 32));
            highest = int256(scratchWord(0x80 + 10 * 32));
            if (provider == address(0)) provider = address(uint160(scratchWord(0x80 + 8 * 32)));
        }
        flags = 16;
        uint256 referenceX64;
        if (provider != address(0) && callReturn(provider, SEL_QUOTE, 0, false, 0x80, 96)) {
            referenceX64 = scratchWord(0x80 + 64);
            flags = 0;
        }
        ptr = put(ptr, slot0Word);
        ptr = put(ptr, scale0);
        ptr = put(ptr, scale1);
        ptr = put(ptr, uint256(lowest));
        ptr = put(ptr, uint256(highest));
        ptr = put(ptr, referenceX64);
        ptr = put(ptr, uint256(uint160(provider)));
        if (!okImm || highest < lowest) {
            flags |= 2;
            highest = -1;
        }
        return (ptr, lowest, highest, flags);
    }

    function writeMetricBins(uint256 ptr, address pool, int256 lowest, int256 highest)
        internal
        view
        returns (uint256, uint256 flags)
    {
        if (gasleft() < 300_000) {
            ptr = put(ptr, uint256(lowest));
            ptr = put(ptr, 0);
            return (ptr, FLAG_TRUNCATED);
        }
        uint256 span = uint256(highest - lowest) + 1;
        uint256 n = span;
        uint256 next = SCAN_ALL;
        if (n > MAX_BINS) {
            n = MAX_BINS;
            flags = FLAG_TRUNCATED;
            next = uint256(lowest + int256(n));
        }
        ptr = put(ptr, next);
        ptr = put(ptr, n);
        for (uint256 i = 0; i < n; i++) {
            int256 bin = lowest + int256(i);
            ptr = put(ptr, uint256(bin));
            ptr = put(ptr, binAt(pool, bin));
        }
        return (ptr, flags);
    }

    function wordRange(int256 spacing, uint256 scanFrom) internal pure returns (int256 start, int256 maxW) {
        int256 minC = sdiv(MIN_TICK, spacing);
        if (smod(MIN_TICK, spacing) != 0) minC -= 1;
        int256 maxC = sdiv(MAX_TICK, spacing);
        start = scanFrom == SCAN_ALL ? sar8(minC) : int256(scanFrom);
        maxW = sar8(maxC);
    }

    function v4State(uint256 id) internal pure returns (uint256 slot) {
        assembly {
            mstore(0x00, id)
            mstore(0x20, 6)
            slot := keccak256(0x00, 0x40)
        }
    }

    function bitmapAt(address manager, uint256 base, int256 word) internal view returns (uint256 bits) {
        uint256 slot;
        assembly {
            mstore(0x00, word)
            mstore(0x20, base)
            slot := keccak256(0x00, 0x40)
        }
        bits = extsload(manager, slot);
    }

    function binAt(address pool, int256 bin) internal view returns (uint256 word) {
        uint256 slot;
        assembly {
            mstore(0x00, bin)
            mstore(0x20, 4)
            slot := keccak256(0x00, 0x40)
        }
        word = extsload(pool, slot);
    }

    /// `extsload(bytes32)` on the pool or the v4 PoolManager. A lens cannot `sload` another account.
    function extsload(address account, uint256 slot) internal view returns (uint256 word) {
        assembly {
            mstore(0x00, shl(224, 0x1e2eaeaf))
            mstore(0x04, slot)
            let ok := staticcall(200000, account, 0x00, 0x24, 0x00, 0x20)
            word := mload(0x00)
            if iszero(ok) { word := 0 }
        }
    }

    function tickBitmap(address pool, int256 word) internal view returns (bool ok, uint256 bits) {
        assembly {
            mstore(0x00, shl(224, SEL_BITMAP))
            mstore(0x04, word)
            ok := staticcall(200000, pool, 0x00, 0x24, 0x00, 0x20)
            bits := mload(0x00)
        }
    }

    function readSlot0(address pool) internal view returns (bool ok, uint256 sqrt, int256 tick) {
        assembly {
            mstore(0x00, shl(224, SEL_SLOT0))
            ok := staticcall(200000, pool, 0x00, 4, 0x00, 0x40)
            sqrt := and(mload(0x00), sub(shl(160, 1), 1))
            tick := signextend(2, mload(0x20))
        }
    }

    function reserves(address pair) internal view returns (bool ok, uint256 r0, uint256 r1, uint256 ts) {
        assembly {
            mstore(0x00, shl(224, SEL_RESERVES))
            ok := staticcall(200000, pair, 0x00, 4, 0x00, 0x60)
            r0 := mload(0x00)
            r1 := mload(0x20)
            ts := mload(0x40)
        }
    }

    function amplification(address pool) internal view returns (bool ok, uint256 amp, uint256 precision) {
        assembly {
            mstore(0x00, shl(224, SEL_AMP))
            ok := staticcall(200000, pool, 0x00, 4, 0x00, 0x60)
            amp := mload(0x00)
            precision := mload(0x40)
        }
    }

    function poolTokens(address vault, uint256 poolId) internal view returns (bool ok, uint256 n) {
        assembly {
            mstore(0x00, shl(224, SEL_POOL_TOKENS))
            mstore(0x04, poolId)
            ok := staticcall(800000, vault, 0x00, 0x24, 0x80, 0x360)
        }
        if (!ok) return (false, 0);
        uint256 tokensAt = scratchWord(0x80);
        uint256 balsAt = scratchWord(0xa0);
        if (tokensAt > 0x300 || balsAt > 0x300) return (false, 0);
        n = scratchWord(0x80 + tokensAt);
        if (n > MAX_COINS) n = MAX_COINS;
        for (uint256 i = 0; i < n; i++) {
            uint256 coin = scratchWord(0x80 + tokensAt + 32 + i * 32) & ADDR_MASK;
            uint256 bal = scratchWord(0x80 + balsAt + 32 + i * 32);
            // Park the pairs after the return blob so copyScratch can see them.
            assembly {
                mstore(add(0x400, mul(i, 64)), coin)
                mstore(add(0x420, mul(i, 64)), bal)
            }
        }
        // Move the pairs down to 0x80 for copyScratch.
        for (uint256 i = 0; i < n * 2; i++) {
            uint256 word = scratchWord(0x400 + i * 32);
            assembly {
                mstore(add(0x80, mul(i, 32)), word)
            }
        }
        return (true, n);
    }

    /// Copies a `uint256[]` return into `scratch`. `n` is the array length, capped.
    function copyArray(address target, uint256 sel, uint256 arg, bool withArg, uint256 scratch)
        internal
        view
        returns (bool ok, uint256 n)
    {
        ok = callReturn(target, sel, arg, withArg, scratch, 0x300);
        if (!ok) return (false, 0);
        uint256 offset = scratchWord(scratch);
        if (offset > 0x2c0) return (false, 0);
        n = scratchWord(scratch + offset);
        if (n > MAX_COINS) n = MAX_COINS;
        for (uint256 i = 0; i < n; i++) {
            uint256 word = scratchWord(scratch + offset + 32 + i * 32);
            assembly {
                mstore(add(scratch, mul(i, 32)), word)
            }
        }
    }

    function callReturn(address target, uint256 sel, uint256 arg, bool withArg, uint256 scratch, uint256 outSize)
        internal
        view
        returns (bool ok)
    {
        assembly {
            mstore(0x00, shl(224, sel))
            let size := 4
            if withArg {
                mstore(0x04, arg)
                size := 0x24
            }
            ok := staticcall(800000, target, 0x00, size, scratch, outSize)
        }
    }

    function callWord(address target, uint256 sel, uint256 arg, bool withArg)
        internal
        view
        returns (bool ok, uint256 word)
    {
        assembly {
            mstore(0x00, shl(224, sel))
            let size := 4
            if withArg {
                mstore(0x04, arg)
                size := 0x24
            }
            ok := staticcall(400000, target, 0x00, size, 0x00, 0x20)
            word := mload(0x00)
        }
    }

    function copyScratch(uint256 ptr, uint256 n) internal pure returns (uint256) {
        for (uint256 i = 0; i < n; i++) {
            ptr = put(ptr, scratchWord(0x80 + i * 32));
        }
        return ptr;
    }

    function copyFrom(uint256 ptr, uint256 scratch, uint256 n) internal pure returns (uint256) {
        for (uint256 i = 0; i < n; i++) {
            ptr = put(ptr, scratchWord(scratch + i * 32));
        }
        return ptr;
    }

    function scratchWord(uint256 at) internal pure returns (uint256 word) {
        assembly {
            word := mload(at)
        }
    }

    function put(uint256 ptr, uint256 word) internal pure returns (uint256) {
        assembly {
            mstore(ptr, word)
        }
        return ptr + 32;
    }

    function sign24(uint256 word) internal pure returns (int256 value) {
        assembly {
            value := signextend(2, word)
        }
    }

    function sdiv(int256 a, int256 b) internal pure returns (int256 q) {
        assembly {
            q := sdiv(a, b)
        }
    }

    function smod(int256 a, int256 b) internal pure returns (int256 r) {
        assembly {
            r := smod(a, b)
        }
    }

    function sar8(int256 value) internal pure returns (int256 out) {
        assembly {
            out := sar(8, value)
        }
    }
}
