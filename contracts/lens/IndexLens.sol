// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

/// Enumerates one slice of an on-chain pool index inside a single `eth_call`.
///
/// The call has no `to`. `data` is this contract's creation code with ABI-encoded
/// constructor arguments appended. The constructor reverts with the rows, so the
/// client reads JSON-RPC error `data` and does not deploy anything.
///
/// Compiled with solc 0.8.28, optimizer on, 200 runs. The creation hex next to
/// this file is what the Rust client appends arguments to.
///
/// Payload, each value one 32-byte word:
///
/// ```
/// magic, count, row[count]
/// ```
///
/// `magic` is keccak256("ratatoskr.lens.v1").
///
/// mode 0: address
/// mode 1: address, token0, token1
/// mode 2: address, token0, token1, fee, tickSpacing
///         fee is 2^256-1 when `fee()` reverts. tickSpacing is 2^255-1 when
///         `tickSpacing()` reverts. A reverted token call is address(0).
/// mode 3: address, nCoins, coin0 .. coin7
///         `coins(uint256)`, then `coins(int128)` if the first call reverts.
contract IndexLens {
    uint256 internal constant MAGIC =
        0xaee64a0fcd28252b5f7449ae9833b0c8c478d83a2e266dbb8668406d5e8d2667;
    uint256 internal constant ADDR_MASK = (1 << 160) - 1;
    bytes4 internal constant TOKEN0 = 0x0dfe1681;
    bytes4 internal constant TOKEN1 = 0xd21220a7;
    bytes4 internal constant FEE = 0xddca3f43;
    bytes4 internal constant TICK = 0xd0c93a7c;
    bytes4 internal constant COINS_U256 = 0xc6610657;
    bytes4 internal constant COINS_I128 = 0x23746eb8;

    constructor(address factory, bytes4 indexSel, uint256 start, uint256 count, uint8 mode) {
        if (mode > 3 || count > 4096) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 width = 1;
        if (mode == 1) width = 3;
        if (mode == 2) width = 5;
        if (mode == 3) width = 10;

        uint256 total = (2 + count * width) * 32;
        bytes memory out;
        assembly {
            out := mload(0x40)
            mstore(out, total)
            mstore(add(out, 32), MAGIC)
            mstore(add(out, 64), count)
            mstore(0x40, add(add(out, 32), total))
        }

        for (uint256 i = 0; i < count; i++) {
            address pool = callAddress(factory, uint256(uint32(indexSel)), start + i);
            uint256 ptr;
            assembly {
                ptr := add(add(out, 96), mul(i, mul(width, 32)))
            }
            if (mode == 0) {
                storeAddress(ptr, pool);
            } else if (mode == 1) {
                storePair(ptr, pool);
            } else if (mode == 2) {
                storePair(ptr, pool);
                storeFeeTick(ptr, pool);
            } else {
                storeCoins(ptr, pool);
            }
        }

        assembly {
            revert(add(out, 32), mload(out))
        }
    }

    /// `sel` is the numeric selector, not a left-aligned bytes4 word.
    function callAddress(address target, uint256 sel, uint256 arg) internal view returns (address result) {
        assembly {
            mstore(0x00, shl(224, sel))
            mstore(0x04, arg)
            let ok := staticcall(500000, target, 0x00, 0x24, 0x00, 0x20)
            if ok {
                result := and(mload(0x00), ADDR_MASK)
            }
        }
    }

    /// `sel` is the numeric selector. `withArg` appends one uint256.
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
            ok := staticcall(300000, target, 0x00, size, 0x00, 0x20)
            word := mload(0x00)
        }
    }

    function storeAddress(uint256 ptr, address value) internal pure {
        assembly {
            mstore(ptr, value)
        }
    }

    function storePair(uint256 ptr, address pool) internal view {
        storeAddress(ptr, pool);
        if (pool == address(0)) return;
        (bool ok0, uint256 w0) = callWord(pool, uint256(uint32(TOKEN0)), 0, false);
        (bool ok1, uint256 w1) = callWord(pool, uint256(uint32(TOKEN1)), 0, false);
        storeAddress(ptr + 32, ok0 ? address(uint160(w0)) : address(0));
        storeAddress(ptr + 64, ok1 ? address(uint160(w1)) : address(0));
    }

    function storeFeeTick(uint256 ptr, address pool) internal view {
        uint256 fee = type(uint256).max;
        uint256 tick = uint256(type(int256).max);
        if (pool != address(0)) {
            (bool okF, uint256 wF) = callWord(pool, uint256(uint32(FEE)), 0, false);
            if (okF) fee = wF;
            (bool okT, uint256 wT) = callWord(pool, uint256(uint32(TICK)), 0, false);
            if (okT) tick = wT;
        }
        assembly {
            mstore(add(ptr, 96), fee)
            mstore(add(ptr, 128), tick)
        }
    }

    function storeCoins(uint256 ptr, address pool) internal view {
        storeAddress(ptr, pool);
        if (pool == address(0)) return;
        uint256 sel = uint256(uint32(COINS_U256));
        (bool ok, uint256 first) = callWord(pool, sel, 0, true);
        if (!ok) {
            sel = uint256(uint32(COINS_I128));
            (ok, first) = callWord(pool, sel, 0, true);
        }
        if (!ok) return;
        uint256 n = 1;
        storeAddress(ptr + 64, address(uint160(first)));
        for (uint256 c = 1; c < 8; c++) {
            (bool next, uint256 word) = callWord(pool, sel, c, true);
            if (!next) break;
            storeAddress(ptr + 64 + c * 32, address(uint160(word)));
            n = c + 1;
        }
        assembly {
            mstore(add(ptr, 32), n)
        }
    }
}
