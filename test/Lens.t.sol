// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {DetailLens} from "lens/DetailLens.sol";
import {IndexLens} from "lens/IndexLens.sol";
import {TickLens} from "lens/TickLens.sol";

/// Creation-code lenses revert their payload out of the constructor.
contract LensTest is Test {
    uint256 internal constant EIP3860_INITCODE = 49152;

    function testCreationCodeFitsTheInitcodeLimit() public pure {
        assertLt(type(IndexLens).creationCode.length, EIP3860_INITCODE);
        assertLt(type(DetailLens).creationCode.length, EIP3860_INITCODE);
        assertLt(type(TickLens).creationCode.length, EIP3860_INITCODE);
    }

    function testCreationCodePushesTheLensMagic() public pure {
        assertTrue(_pushes(type(IndexLens).creationCode, keccak256("ratatoskr.lens.v1")));
        assertTrue(_pushes(type(DetailLens).creationCode, keccak256("ratatoskr.detail.v1")));
        assertTrue(_pushes(type(TickLens).creationCode, keccak256("ratatoskr.tick.v1")));
    }

    function testIndexLensCountZeroRevertsTheMagic() public {
        try new IndexLens(address(0), bytes4(0), 0, 0, 0) {
            fail();
        } catch (bytes memory data) {
            assertEq(_word(data, 0), keccak256("ratatoskr.lens.v1"));
            assertEq(_word(data, 1), bytes32(0));
        }
    }

    function testDetailLensRejectsAShortArgument() public {
        uint256[] memory items = new uint256[](1);
        try new DetailLens(items) {
            fail();
        } catch (bytes memory data) {
            assertEq(data.length, 0);
        }
    }

    function testDetailLensProductRevertsTheMagic() public {
        uint256[] memory items = new uint256[](8);
        items[0] = 1;
        items[1] = uint256(uint160(address(0xBEEF)));
        items[2] = uint256(uint160(address(0xBEEF)));
        try new DetailLens(items) {
            fail();
        } catch (bytes memory data) {
            assertEq(_word(data, 0), keccak256("ratatoskr.detail.v1"));
            assertEq(_word(data, 1), bytes32(uint256(1)));
        }
    }

    function testTickLensEmptyPoolRevertsTheMagic() public {
        uint256[] memory items = new uint256[](6);
        items[0] = 1;
        items[1] = 5;
        items[2] = uint256(keccak256("pool"));
        items[3] = uint256(uint160(address(0xBEEF)));
        try new TickLens(items) {
            fail();
        } catch (bytes memory data) {
            assertEq(_word(data, 0), keccak256("ratatoskr.tick.v1"));
            assertEq(_word(data, 1), bytes32(uint256(1)));
        }
    }

    function _pushes(bytes memory code, bytes32 word) internal pure returns (bool) {
        uint256 length = code.length;
        for (uint256 i; i + 33 <= length; ++i) {
            if (code[i] != 0x7f) continue;
            bytes32 got;
            assembly {
                got := mload(add(add(code, 0x21), i))
            }
            if (got == word) return true;
        }
        return false;
    }

    function _word(bytes memory data, uint256 index) internal pure returns (bytes32 word) {
        require(data.length >= (index + 1) * 32, "short");
        assembly {
            word := mload(add(add(data, 0x20), mul(index, 0x20)))
        }
    }
}
