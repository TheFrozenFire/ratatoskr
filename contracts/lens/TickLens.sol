// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

/// One `eth_call` of this creation code reads initialized tick words for many pools.
///
/// The call has no `to`. The constructor reverts the payload. Nothing is deployed.
/// Compiled with solc 0.8.28, optimizer on, 200 runs.
///
/// This is the second read, after `DetailLens` has returned the non-zero bitmap
/// words. Tick reads stay out of that call so the two payloads do not share a gas
/// limit or a response-size budget. The client fills each call up to the provider's
/// gas and response cap, mixing pools in one batch.
///
/// Constructor argument is one `uint256[]`:
///
/// ```
/// poolCount,
///   kind, id, home, slotBase, nTicks, tick_0 .. tick_{nTicks-1},
///   ...
/// ```
///
/// Kind 10 (Uniswap v4) reads `extsload` of `keccak256(abi.encode(int256(tick), slotBase))`
/// on `home` (the PoolManager). Every other concentrated kind staticcalls `ticks(int24)`
/// and packs `liquidityGross` (low 128) with `liquidityNet` (high 128).
///
/// The lens stops when it has written `MAX_TICKS` words or `gasleft()` is low.
/// Later pools in that call come back with `written == 0` and the truncated flag.
/// A failed `ticks()` staticcall stores a zero word and sets the failed flag.
///
/// Revert payload, 32-byte words:
///
/// ```
/// magic, poolCount, record[poolCount]
/// ```
///
/// `magic` is keccak256("ratatoskr.tick.v1").
/// Each record is `nWords`, `id`, `flags`, `written`, then `written` pairs of
/// `(tick, packed)`. `flags` bit 0 means this pool did not finish. Bit 1 means a
/// tick staticcall failed.
contract TickLens {
    uint256 internal constant MAGIC =
        0xfcba65b20d7c4d16203a3cd71d895ce70ff688c2424eae5d34a5d0bed06b5d0f;
    uint256 internal constant MAX_TICKS = 1024;
    uint256 internal constant MAX_POOLS = 512;
    uint256 internal constant FLAG_TRUNCATED = 1;
    uint256 internal constant FLAG_FAILED = 2;
    uint256 internal constant KIND_V4 = 10;
    uint256 internal constant SEL_TICKS = 0xf30dba93;

    constructor(uint256[] memory items) {
        if (items.length == 0) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 pools = items[0];
        if (pools == 0 || pools > MAX_POOLS) {
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
        uint256 cursor = 1;
        uint256 room = MAX_TICKS;
        for (uint256 i = 0; i < pools; i++) {
            (ptr, cursor, room) = writePool(ptr, items, cursor, room);
        }
        if (cursor != items.length) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 total = ptr - base;
        assembly {
            mstore(base, sub(total, 32))
            mstore(add(base, 32), MAGIC)
            mstore(add(base, 64), pools)
            revert(add(base, 32), mload(base))
        }
    }

    /// Returns the advanced output pointer, the advanced input cursor, and the ticks still allowed.
    ///
    /// Pool fields live at 0x80..0x120 so this function stays inside the stack limit.
    /// Output starts at 0x800 or later, past that scratch.
    function writePool(uint256 ptr, uint256[] memory items, uint256 cursor, uint256 room)
        internal
        view
        returns (uint256, uint256, uint256)
    {
        uint256 id = stage(items, cursor);
        uint256 start = ptr;
        ptr = put(ptr, 0);
        ptr = put(ptr, id);
        ptr = put(ptr, 0);
        ptr = put(ptr, 0);
        (ptr, room) = writeTicks(ptr, start, items, room);
        cursor = scratch(0x120);
        return (ptr, cursor, room);
    }

    /// Stores kind, home, slot base, tick count, and the cursor after this pool. Returns `id`.
    function stage(uint256[] memory items, uint256 cursor) internal pure returns (uint256 id) {
        if (cursor + 5 > items.length) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 n = items[cursor + 4];
        if (n > MAX_TICKS || cursor + 5 + n > items.length) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 kind = items[cursor];
        id = items[cursor + 1];
        uint256 home = items[cursor + 2];
        uint256 slotBase = items[cursor + 3];
        uint256 next = cursor + 5 + n;
        assembly {
            mstore(0x80, kind)
            mstore(0xa0, home)
            mstore(0xc0, slotBase)
            mstore(0xe0, n)
            mstore(0x100, add(cursor, 5))
            mstore(0x120, next)
        }
    }

    function writeTicks(uint256 ptr, uint256 start, uint256[] memory items, uint256 room)
        internal
        view
        returns (uint256, uint256)
    {
        uint256 n = scratch(0xe0);
        uint256 cursor = scratch(0x100);
        uint256 written;
        uint256 flags;
        if (room == 0) {
            flags = FLAG_TRUNCATED;
        } else {
            while (written < n && room != 0) {
                if (gasleft() < 400_000) {
                    room = 0;
                    break;
                }
                uint256 failed;
                (ptr, failed) = appendTick(ptr, items[cursor + written]);
                flags |= failed;
                written++;
                room--;
            }
            if (written < n) flags |= FLAG_TRUNCATED;
        }
        assembly {
            mstore(start, div(sub(ptr, start), 32))
            mstore(add(start, 64), flags)
            mstore(add(start, 96), written)
        }
        return (ptr, room);
    }

    function appendTick(uint256 ptr, uint256 rawTick) internal view returns (uint256, uint256 failed) {
        int256 tick = asInt(rawTick);
        bool ok;
        uint256 packed;
        if (scratch(0x80) == KIND_V4) {
            (ok, packed) = readTickStorage(address(uint160(scratch(0xa0))), scratch(0xc0), tick);
        } else {
            (ok, packed) = readTickView(address(uint160(scratch(0xa0))), tick);
        }
        if (!ok) failed = FLAG_FAILED;
        ptr = putInt(ptr, tick);
        ptr = put(ptr, ok ? packed : 0);
        return (ptr, failed);
    }

    function scratch(uint256 at) internal pure returns (uint256 word) {
        assembly {
            word := mload(at)
        }
    }

    /// `ticks(int24)` returns gross and net in separate words. Storage packs them into one.
    function readTickView(address pool, int256 tick) internal view returns (bool ok, uint256 packed) {
        assembly {
            mstore(0x00, shl(224, SEL_TICKS))
            mstore(0x04, tick)
            ok := staticcall(200000, pool, 0x00, 0x24, 0x00, 0x40)
            if ok {
                let gross := and(mload(0x00), sub(shl(128, 1), 1))
                let net := and(mload(0x20), sub(shl(128, 1), 1))
                packed := or(gross, shl(128, net))
            }
        }
    }

    function readTickStorage(address manager, uint256 slotBase, int256 tick)
        internal
        view
        returns (bool ok, uint256 packed)
    {
        assembly {
            mstore(0x00, tick)
            mstore(0x20, slotBase)
            let slot := keccak256(0x00, 0x40)
            mstore(0x00, shl(224, 0x1e2eaeaf))
            mstore(0x04, slot)
            ok := staticcall(200000, manager, 0x00, 0x24, 0x00, 0x20)
            packed := mload(0x00)
            if iszero(ok) { packed := 0 }
        }
    }

    function asInt(uint256 word) internal pure returns (int256 value) {
        assembly {
            value := word
        }
    }

    function put(uint256 ptr, uint256 word) internal pure returns (uint256) {
        assembly {
            mstore(ptr, word)
        }
        return ptr + 32;
    }

    function putInt(uint256 ptr, int256 word) internal pure returns (uint256) {
        assembly {
            mstore(ptr, word)
        }
        return ptr + 32;
    }
}
