// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

/// Reads `symbol()` and `decimals()` for many tokens in one `eth_call`.
///
/// The call has no `to`. `data` is this contract's creation code with the ABI
/// encoding of `address[]` appended. The constructor reverts the rows, so the
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
/// A row is `address, decimals, symbol`. `decimals` is `2^256-1` when the call
/// reverts or the value does not fit in `uint8`. `symbol` is left-aligned in a
/// bytes32, truncated at 32 bytes. An empty word means `symbol()` reverted or
/// returned empty. A `string` return and a `bytes32` return are both accepted.
///
/// `magic` is keccak256("ratatoskr.token.v1").
contract TokenLens {
    uint256 internal constant MAGIC = 0x4e3da3df77626eed44d6bf65c4867d111359d65b6d3c5c7daec41bfb6fd15583;
    uint256 internal constant STIPEND = 500000;
    uint256 internal constant DECIMALS = 0x313ce567;
    uint256 internal constant SYMBOL = 0x95d89b41;

    constructor(address[] memory tokens) {
        uint256 count = tokens.length;
        if (count > 256) {
            assembly {
                revert(0, 0)
            }
        }
        uint256 total = (2 + count * 3) * 32;
        bytes memory out;
        assembly {
            out := mload(0x40)
            mstore(out, total)
            mstore(add(out, 32), MAGIC)
            mstore(add(out, 64), count)
            mstore(0x40, add(add(out, 32), total))
        }
        for (uint256 i = 0; i < count; i++) {
            address token = tokens[i];
            uint256 ptr;
            assembly {
                ptr := add(add(out, 96), mul(i, 96))
            }
            store(ptr, uint256(uint160(token)));
            store(ptr + 32, decimalsOf(token));
            store(ptr + 64, symbolOf(token));
        }
        assembly {
            revert(add(out, 32), mload(out))
        }
    }

    function decimalsOf(address token) internal view returns (uint256 value) {
        assembly {
            value := not(0)
            mstore(0x00, shl(224, DECIMALS))
            mstore(0x80, 0)
            let ok := staticcall(STIPEND, token, 0x00, 4, 0x80, 0x20)
            if and(ok, gt(returndatasize(), 31)) {
                let got := mload(0x80)
                if lt(got, 256) { value := got }
            }
        }
    }

    /// `string` is an offset of `0x20` followed by a length. Anything else is `bytes32`.
    function symbolOf(address token) internal view returns (uint256 word) {
        assembly {
            mstore(0x00, shl(224, SYMBOL))
            mstore(0x80, 0)
            mstore(0xa0, 0)
            mstore(0xc0, 0)
            let ok := staticcall(STIPEND, token, 0x00, 4, 0x80, 0x60)
            if and(ok, gt(returndatasize(), 31)) {
                let first := mload(0x80)
                if and(eq(first, 0x20), gt(returndatasize(), 63)) {
                    let len := mload(0xa0)
                    if gt(len, 32) { len := 32 }
                    word := mload(0xc0)
                    if lt(len, 32) {
                        let drop := mul(sub(32, len), 8)
                        word := and(word, shl(drop, not(0)))
                    }
                }
                if iszero(eq(first, 0x20)) { word := first }
            }
        }
    }

    function store(uint256 ptr, uint256 value) internal pure {
        assembly {
            mstore(ptr, value)
        }
    }
}
