use std::collections::BTreeSet;

use super::*;

fn token(byte: u8) -> Token {
    Token([byte; 20])
}

fn address(byte: u8) -> Address {
    Address::new([byte; 20]).expect("address")
}

fn mint(tokens: Vec<Token>, shape: ShareShape) -> Result<MintLiquidity, ActionError> {
    MintLiquidity::new(address(8), address(8).pool_id(), token(9), tokens, shape)
}

#[test]
fn a_native_wrap_spends_eth_and_pays_the_wrapper() {
    let wrapper = token(1);
    let action = Action::Wrap(Wrap::new(wrapper, NATIVE).expect("wrap"));
    assert_eq!(action.spends(), vec![NATIVE]);
    assert_eq!(action.pays(), vec![wrapper]);
}

#[test]
fn unwrapping_pays_the_underlying() {
    let wrapper = token(1);
    let wrap = Action::Wrap(Wrap::new(wrapper, NATIVE).expect("wrap"));
    let unwrap = Action::Unwrap(Unwrap::new(wrapper, NATIVE).expect("unwrap"));
    assert_eq!(unwrap.spends(), wrap.pays());
    assert_eq!(unwrap.pays(), wrap.spends());
}

#[test]
fn a_wrap_rejects_native_as_the_wrapper() {
    let err = Wrap::new(NATIVE, token(1)).expect_err("native wrapper");
    assert_eq!(err, ActionError::NativeToken);
    assert_eq!(err.to_string(), "the token is native ETH");
}

#[test]
fn a_wrap_rejects_one_token_on_both_sides() {
    let err = Wrap::new(token(1), token(1)).expect_err("same token");
    assert_eq!(err, ActionError::SameToken);
}

#[test]
fn a_rate_wrap_rejects_native_eth() {
    let err = RateWrap::new(token(1), NATIVE).expect_err("native underlying");
    assert_eq!(err, ActionError::NativeToken);
    let action = Action::RateWrap(RateWrap::new(token(1), token(2)).expect("rate"));
    assert_eq!(action.spends(), vec![token(2)]);
    assert_eq!(action.pays(), vec![token(1)]);
    let back = Action::RateUnwrap(RateUnwrap::new(token(1), token(2)).expect("rate"));
    assert_eq!(back.spends(), vec![token(1)]);
    assert_eq!(back.pays(), vec![token(2)]);
}

#[test]
fn a_factory_pool_id_keeps_the_address_in_the_low_bytes() {
    let contract = address(0xab);
    let id = contract.pool_id();
    assert_eq!(&id[..12], &[0; 12]);
    assert_eq!(&id[12..], &contract.0);
}

#[test]
fn a_swap_rejects_a_zero_contract() {
    let err = Swap::new(
        Address([0; 20]),
        [1; 32],
        token(2),
        token(3),
        Invariant::Product,
    )
    .expect_err("zero contract");
    assert_eq!(err, ActionError::ZeroContract);
}

#[test]
fn a_swap_rejects_a_zero_pool() {
    let err = Swap::new(address(1), [0; 32], token(2), token(3), Invariant::Product)
        .expect_err("zero pool");
    assert_eq!(err, ActionError::ZeroPool);
}

#[test]
fn a_swap_keeps_its_direction() {
    let pool = address(1).pool_id();
    let forward =
        Swap::new(address(1), pool, token(2), token(3), Invariant::Product).expect("swap");
    let back = Swap::new(address(1), pool, token(3), token(2), Invariant::Product).expect("swap");
    assert_ne!(forward, back);
    assert_eq!(Action::Swap(forward).spends(), vec![token(2)]);
    assert_eq!(Action::Swap(forward).pays(), vec![token(3)]);
}

#[test]
fn a_swap_can_spend_native_eth() {
    let action = Action::Swap(
        Swap::new(
            address(1),
            [9; 32],
            NATIVE,
            token(2),
            Invariant::Concentrated,
        )
        .expect("swap"),
    );
    assert_eq!(action.spends(), vec![NATIVE]);
    assert_eq!(action.pays(), vec![token(2)]);
}

#[test]
fn a_concentrated_swap_accepts_a_singleton_pool_id() {
    let mut pool = [0x11; 32];
    pool[0] = 0xff;
    let swap = Swap::new(
        address(1),
        pool,
        token(2),
        token(3),
        Invariant::Concentrated,
    )
    .expect("swap");
    assert_eq!(swap.pool, pool);
}

#[test]
fn the_quoting_rule_is_part_of_the_swap() {
    let pool = [4; 32];
    let product =
        Swap::new(address(1), pool, token(2), token(3), Invariant::Product).expect("swap");
    let bins = Swap::new(address(1), pool, token(2), token(3), Invariant::Bin).expect("swap");
    let pmm = Swap::new(address(1), pool, token(2), token(3), Invariant::Pmm).expect("swap");
    assert_ne!(product, bins);
    assert_ne!(bins, pmm);
}

#[test]
fn liquidity_mints_with_swapped_tokens_are_equal() {
    let left = mint(vec![token(3), token(2)], ShareShape::Proportional).expect("mint");
    let right = mint(vec![token(2), token(3)], ShareShape::Proportional).expect("mint");
    assert_eq!(left, right);
    assert_eq!(left.tokens, vec![token(2), token(3)]);
    let set: BTreeSet<_> = [Action::MintLiquidity(left), Action::MintLiquidity(right)]
        .into_iter()
        .collect();
    assert_eq!(set.len(), 1);
}

#[test]
fn proportional_and_unbalanced_mints_differ() {
    let proportional = mint(vec![token(2), token(3)], ShareShape::Proportional).expect("mint");
    let unbalanced = mint(vec![token(2), token(3)], ShareShape::Unbalanced).expect("mint");
    assert_ne!(proportional, unbalanced);
}

#[test]
fn a_multi_token_shape_rejects_one_token() {
    assert_eq!(
        mint(vec![token(2)], ShareShape::Proportional).expect_err("one"),
        ActionError::Count
    );
    assert_eq!(
        mint(vec![token(2)], ShareShape::Unbalanced).expect_err("one"),
        ActionError::Count
    );
}

#[test]
fn a_single_mint_rejects_two_tokens() {
    let err = mint(vec![token(2), token(3)], ShareShape::Single).expect_err("two");
    assert_eq!(err, ActionError::Count);
}

#[test]
fn a_single_token_mint_spends_that_token() {
    let action = Action::MintLiquidity(mint(vec![token(2)], ShareShape::Single).expect("mint"));
    assert_eq!(action.spends(), vec![token(2)]);
    assert_eq!(action.pays(), vec![token(9)]);
}

#[test]
fn a_mint_rejects_the_share_among_the_deposits() {
    let err = mint(vec![token(9), token(2)], ShareShape::Proportional).expect_err("share");
    assert_eq!(err, ActionError::ShareInPool);
}

#[test]
fn a_mint_rejects_a_repeated_token() {
    let err = mint(vec![token(2), token(2)], ShareShape::Proportional).expect_err("repeat");
    assert_eq!(err, ActionError::DuplicateToken);
}

#[test]
fn a_mint_rejects_native_eth() {
    let err = mint(vec![NATIVE, token(2)], ShareShape::Proportional).expect_err("native");
    assert_eq!(err, ActionError::NativeToken);
}

#[test]
fn a_mint_rejects_a_zero_pool() {
    let err = MintLiquidity::new(
        address(8),
        [0; 32],
        token(9),
        vec![token(2), token(3)],
        ShareShape::Proportional,
    )
    .expect_err("zero pool");
    assert_eq!(err, ActionError::ZeroPool);
}

#[test]
fn a_pair_mint_can_use_the_pair_as_the_share() {
    let pair = address(7);
    let share = Token(pair.0);
    let mint = MintLiquidity::new(
        pair,
        pair.pool_id(),
        share,
        vec![token(1), token(2)],
        ShareShape::Proportional,
    )
    .expect("pair");
    assert_eq!(mint.share.0, mint.contract.0);
    assert_eq!(Action::MintLiquidity(mint).pays(), vec![share]);
}

#[test]
fn burning_spends_the_share_and_pays_the_tokens() {
    let burn = BurnLiquidity::new(
        address(8),
        address(8).pool_id(),
        token(9),
        vec![token(3), token(2)],
        ShareShape::Proportional,
    )
    .expect("burn");
    let action = Action::BurnLiquidity(burn);
    assert_eq!(action.spends(), vec![token(9)]);
    assert_eq!(action.pays(), vec![token(2), token(3)]);
}

#[test]
fn a_vault_deposit_rejects_native_eth() {
    let err = DepositVault::new(token(1), NATIVE).expect_err("native asset");
    assert_eq!(err, ActionError::NativeToken);
}

#[test]
fn redeeming_a_vault_pays_the_asset() {
    let deposit = Action::DepositVault(DepositVault::new(token(1), token(2)).expect("vault"));
    let redeem = Action::RedeemVault(RedeemVault::new(token(1), token(2)).expect("vault"));
    assert_eq!(deposit.spends(), vec![token(2)]);
    assert_eq!(deposit.pays(), vec![token(1)]);
    assert_eq!(redeem.spends(), deposit.pays());
    assert_eq!(redeem.pays(), deposit.spends());
}

#[test]
fn supplying_pays_the_rebasing_receipt() {
    let action = Action::Supply(Supply::new(address(1), token(2), token(3)).expect("supply"));
    assert_eq!(action.spends(), vec![token(2)]);
    assert_eq!(action.pays(), vec![token(3)]);
}

#[test]
fn withdrawing_pays_the_asset() {
    let action = Action::Withdraw(Withdraw::new(address(1), token(2), token(3)).expect("withdraw"));
    assert_eq!(action.spends(), vec![token(3)]);
    assert_eq!(action.pays(), vec![token(2)]);
}

#[test]
fn supplying_is_not_staking() {
    let supply = Action::Supply(Supply::new(address(1), token(2), token(3)).expect("supply"));
    let stake = Action::Stake(Stake::new(address(1), token(2), token(3)).expect("stake"));
    assert_ne!(supply, stake);
}

#[test]
fn staking_native_eth_can_name_the_receipt_as_the_protocol() {
    let receipt = token(4);
    let protocol = Address::new(receipt.0).expect("protocol");
    let action = Action::Stake(Stake::new(protocol, NATIVE, receipt).expect("stake"));
    assert_eq!(action.spends(), vec![NATIVE]);
    assert_eq!(action.pays(), vec![receipt]);
}

#[test]
fn unstaking_pays_the_asset() {
    let action = Action::Unstake(Unstake::new(address(4), token(2), token(5)).expect("unstake"));
    assert_eq!(action.spends(), vec![token(5)]);
    assert_eq!(action.pays(), vec![token(2)]);
}

#[test]
fn a_stake_rejects_the_asset_as_the_receipt() {
    let err = Stake::new(address(1), token(2), token(2)).expect_err("same");
    assert_eq!(err, ActionError::SameToken);
}

#[test]
fn a_peg_convert_differs_from_a_migration() {
    let peg = Convert::new(address(1), token(2), token(3), ConvertMode::Peg).expect("peg");
    let migrate =
        Convert::new(address(1), token(2), token(3), ConvertMode::Migrate).expect("migrate");
    assert_ne!(peg, migrate);
    let action = Action::Convert(peg);
    assert_eq!(action.spends(), vec![token(2)]);
    assert_eq!(action.pays(), vec![token(3)]);
}

#[test]
fn a_migration_keeps_its_direction() {
    let forward =
        Convert::new(address(1), token(2), token(3), ConvertMode::Migrate).expect("forward");
    let back = Convert::new(address(1), token(3), token(2), ConvertMode::Migrate).expect("back");
    assert_ne!(forward, back);
}

#[test]
fn a_fill_is_not_a_swap() {
    let fill =
        Action::Fill(Fill::new(address(1), vec![token(2)], vec![token(3)], [7; 32]).expect("fill"));
    let swap = Action::Swap(
        Swap::new(
            address(1),
            address(1).pool_id(),
            token(2),
            token(3),
            Invariant::Product,
        )
        .expect("swap"),
    );
    assert_ne!(fill, swap);
    assert_eq!(fill.spends(), vec![token(2)]);
    assert_eq!(fill.pays(), vec![token(3)]);
}

#[test]
fn a_fill_sorts_each_side() {
    let fill = Fill::new(
        address(1),
        vec![token(5), token(4)],
        vec![token(3), token(2)],
        [9; 32],
    )
    .expect("fill");
    assert_eq!(fill.spend, vec![token(4), token(5)]);
    assert_eq!(fill.pay, vec![token(2), token(3)]);
}

#[test]
fn a_fill_rejects_a_token_on_both_sides() {
    let err = Fill::new(
        address(1),
        vec![token(2), token(3)],
        vec![token(3)],
        [1; 32],
    )
    .expect_err("overlap");
    assert_eq!(err, ActionError::SameToken);
}

#[test]
fn a_fill_rejects_an_empty_side() {
    let err = Fill::new(address(1), vec![], vec![token(2)], [1; 32]).expect_err("empty");
    assert_eq!(err, ActionError::EmptySide);
}

#[test]
fn a_fill_may_name_a_zero_order() {
    let fill = Fill::new(address(1), vec![NATIVE], vec![token(2)], [0; 32]).expect("order");
    assert_eq!(fill.order, [0; 32]);
    assert_eq!(Action::Fill(fill).spends(), vec![NATIVE]);
}

#[test]
fn two_orders_are_different_fills() {
    let left = Fill::new(address(1), vec![token(2)], vec![token(3)], [1; 32]).expect("fill");
    let right = Fill::new(address(1), vec![token(2)], vec![token(3)], [2; 32]).expect("fill");
    assert_ne!(left, right);
}

#[test]
fn a_split_sorts_the_claims() {
    let split = Split::new(address(1), token(2), vec![token(4), token(3)]).expect("split");
    assert_eq!(split.outputs, vec![token(3), token(4)]);
    let action = Action::Split(split);
    assert_eq!(action.spends(), vec![token(2)]);
    assert_eq!(action.pays(), vec![token(3), token(4)]);
}

#[test]
fn merging_spends_both_claims() {
    let merge = Merge::new(address(1), vec![token(4), token(3)], token(2)).expect("merge");
    let action = Action::Merge(merge);
    assert_eq!(action.spends(), vec![token(3), token(4)]);
    assert_eq!(action.pays(), vec![token(2)]);
}

#[test]
fn a_split_rejects_one_claim() {
    let err = Split::new(address(1), token(2), vec![token(3)]).expect_err("one");
    assert_eq!(err, ActionError::Count);
}

#[test]
fn a_split_rejects_the_input_among_the_claims() {
    let err = Split::new(address(1), token(2), vec![token(2), token(3)]).expect_err("input");
    assert_eq!(err, ActionError::SameToken);
}
