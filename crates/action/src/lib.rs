//! Synchronous token transforms a trade can settle.
//!
//! [`Action`] is the closed set. A variant names the tokens it spends, the
//! tokens it pays, and the contract that settles them. Amounts, fees, and
//! reserves are not fields. A list of tokens is stored in address order, so
//! two mints that name the same deposits in a different order are one mint.
//!
//! A custodial wrap holds a token one for one. Native ETH is the zero
//! address. An exchange-rate wrap pays its wrapper at the wrapper's rate.
//! A vault deposit pays ERC-4626 or cToken shares for an ERC-20 asset. Supply
//! pays a rebasing lending receipt. Stake pays a liquid-staking receipt.
//! Those settlements finish in the same transaction.
//!
//! A swap is one pool. [`Invariant`] is the quoting rule: constant product,
//! concentrated liquidity, stableswap, constant weights, a crypto pool,
//! discrete bins, or a proactive market maker. A factory pool stores its
//! address in the low 20 bytes of the pool id. A singleton pool stores the
//! 32-byte id. A hook is part of that id.
//!
//! A liquidity mint deposits a pool's tokens and pays a fungible share. The
//! share is priced in the pool's ratio, from an unbalanced deposit, or from
//! one token. A split pays at least two claims for one token, and a merge
//! pays one token for those claims. An order fill is a settlement paying an
//! order, with no pool curve.
//!
//! Position NFTs stay out of the set, including a concentrated-liquidity
//! position. Transfers, approvals, permits, bridges, queued withdrawals,
//! flash loans, zaps, borrows, repayments, liquidations, perpetuals, options,
//! governance votes, gauge locks, and donations that pay no share stay out.
//! A router command list is several actions from this set.
//!
//! The pool key on a slope segment, in the routing crate, is a different type.
//! This crate does not read the catalog.

#[cfg(test)]
mod tests;

use std::fmt;

/// Twenty-byte asset. The zero address is native ETH.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Token(pub [u8; 20]);

/// Native ETH. It has no contract.
pub const NATIVE: Token = Token([0; 20]);

/// Twenty-byte contract. Constructors reject the zero address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(pub [u8; 20]);

impl Address {
    /// Rejects the zero address.
    pub fn new(bytes: [u8; 20]) -> Result<Self, ActionError> {
        checked_address(Address(bytes))
    }

    /// Pool id of a factory pool: this address in the low 20 bytes.
    #[must_use]
    pub fn pool_id(self) -> [u8; 32] {
        let mut id = [0; 32];
        id[12..].copy_from_slice(&self.0);
        id
    }
}

/// A transform the constructors will not build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionError {
    ZeroContract,
    ZeroPool,
    SameToken,
    DuplicateToken,
    EmptySide,
    ShareInPool,
    Count,
    NativeToken,
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ActionError::ZeroContract => "the contract is the zero address",
            ActionError::ZeroPool => "the pool id is zero",
            ActionError::SameToken => "one token is named on both sides",
            ActionError::DuplicateToken => "a token is repeated",
            ActionError::EmptySide => "a side names no token",
            ActionError::ShareInPool => "the share token is one of the pool tokens",
            ActionError::Count => "the token count does not match this action",
            ActionError::NativeToken => "the token is native ETH",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ActionError {}

/// Quoting rule a swap settles against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Invariant {
    /// Constant product of two reserves.
    Product,
    /// Liquidity between ticks. A hook is part of the pool id.
    Concentrated,
    /// Stableswap, or a stable Balancer pool. A metapool coin swap is this rule.
    Stable,
    /// Constant weights.
    Weighted,
    /// A crypto pool, with its price scales.
    Crypto,
    /// Discrete bins. Metric prices the active bin from an oracle.
    Bin,
    /// A proactive market maker. The pool's quote sets the price.
    Pmm,
}

/// How a fungible share is priced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShareShape {
    /// Every listed token, in the pool's ratio.
    Proportional,
    /// Every listed token, at the mix the pool prices.
    Unbalanced,
    /// One listed token.
    Single,
}

/// Which convert the contract runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConvertMode {
    /// A pegged convert, such as a PSM. The fee stays off the action.
    Peg,
    /// A renaming, such as DAI to USDS. The reverse direction is a separate convert.
    Migrate,
}

/// Custodial wrap. One underlying token, including native ETH, pays one wrapper token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Wrap {
    pub wrapper: Token,
    pub underlying: Token,
}

impl Wrap {
    pub fn new(wrapper: Token, underlying: Token) -> Result<Self, ActionError> {
        let (wrapper, underlying) = custody(wrapper, underlying)?;
        Ok(Wrap {
            wrapper,
            underlying,
        })
    }
}

/// Custodial unwrap. The wrapper token pays the underlying token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Unwrap {
    pub wrapper: Token,
    pub underlying: Token,
}

impl Unwrap {
    pub fn new(wrapper: Token, underlying: Token) -> Result<Self, ActionError> {
        let (wrapper, underlying) = custody(wrapper, underlying)?;
        Ok(Unwrap {
            wrapper,
            underlying,
        })
    }
}

/// Exchange-rate wrap between two ERC-20s. Wrapping stETH into wstETH is this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RateWrap {
    pub wrapper: Token,
    pub underlying: Token,
}

impl RateWrap {
    pub fn new(wrapper: Token, underlying: Token) -> Result<Self, ActionError> {
        let (wrapper, underlying) = rate(wrapper, underlying)?;
        Ok(RateWrap {
            wrapper,
            underlying,
        })
    }
}

/// Exchange-rate unwrap. The wrapper pays the underlying at the wrapper's rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RateUnwrap {
    pub wrapper: Token,
    pub underlying: Token,
}

impl RateUnwrap {
    pub fn new(wrapper: Token, underlying: Token) -> Result<Self, ActionError> {
        let (wrapper, underlying) = rate(wrapper, underlying)?;
        Ok(RateUnwrap {
            wrapper,
            underlying,
        })
    }
}

/// One pool swap. Either token may be native ETH.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Swap {
    pub contract: Address,
    pub pool: [u8; 32],
    pub token_in: Token,
    pub token_out: Token,
    pub invariant: Invariant,
}

impl Swap {
    pub fn new(
        contract: Address,
        pool: [u8; 32],
        token_in: Token,
        token_out: Token,
        invariant: Invariant,
    ) -> Result<Self, ActionError> {
        let contract = checked_address(contract)?;
        let pool = checked_pool(pool)?;
        different(token_in, token_out)?;
        Ok(Swap {
            contract,
            pool,
            token_in,
            token_out,
            invariant,
        })
    }
}

/// Fungible liquidity mint. The share is an ERC-20. The tokens are the pool's own tokens.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MintLiquidity {
    pub contract: Address,
    pub pool: [u8; 32],
    pub share: Token,
    pub tokens: Vec<Token>,
    pub shape: ShareShape,
}

impl MintLiquidity {
    pub fn new(
        contract: Address,
        pool: [u8; 32],
        share: Token,
        tokens: Vec<Token>,
        shape: ShareShape,
    ) -> Result<Self, ActionError> {
        let parts = share_parts(contract, pool, share, tokens, shape)?;
        Ok(MintLiquidity {
            contract: parts.contract,
            pool: parts.pool,
            share: parts.share,
            tokens: parts.tokens,
            shape: parts.shape,
        })
    }
}

/// Fungible liquidity burn. The share pays the listed tokens back.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BurnLiquidity {
    pub contract: Address,
    pub pool: [u8; 32],
    pub share: Token,
    pub tokens: Vec<Token>,
    pub shape: ShareShape,
}

impl BurnLiquidity {
    pub fn new(
        contract: Address,
        pool: [u8; 32],
        share: Token,
        tokens: Vec<Token>,
        shape: ShareShape,
    ) -> Result<Self, ActionError> {
        let parts = share_parts(contract, pool, share, tokens, shape)?;
        Ok(BurnLiquidity {
            contract: parts.contract,
            pool: parts.pool,
            share: parts.share,
            tokens: parts.tokens,
            shape: parts.shape,
        })
    }
}

/// ERC-4626 or cToken deposit. The vault token is the share. The asset is an ERC-20.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DepositVault {
    pub vault: Token,
    pub asset: Token,
}

impl DepositVault {
    pub fn new(vault: Token, asset: Token) -> Result<Self, ActionError> {
        let (vault, asset) = vault_pair(vault, asset)?;
        Ok(DepositVault { vault, asset })
    }
}

/// ERC-4626 or cToken redeem. The shares pay the asset in the same transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RedeemVault {
    pub vault: Token,
    pub asset: Token,
}

impl RedeemVault {
    pub fn new(vault: Token, asset: Token) -> Result<Self, ActionError> {
        let (vault, asset) = vault_pair(vault, asset)?;
        Ok(RedeemVault { vault, asset })
    }
}

/// Rebasing lending supply. The receipt balance grows with the reserve index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Supply {
    pub market: Address,
    pub asset: Token,
    pub receipt: Token,
}

impl Supply {
    pub fn new(market: Address, asset: Token, receipt: Token) -> Result<Self, ActionError> {
        let (market, asset, receipt) = lending(market, asset, receipt)?;
        Ok(Supply {
            market,
            asset,
            receipt,
        })
    }
}

/// Rebasing lending withdraw. The receipt pays the asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Withdraw {
    pub market: Address,
    pub asset: Token,
    pub receipt: Token,
}

impl Withdraw {
    pub fn new(market: Address, asset: Token, receipt: Token) -> Result<Self, ActionError> {
        let (market, asset, receipt) = lending(market, asset, receipt)?;
        Ok(Withdraw {
            market,
            asset,
            receipt,
        })
    }
}

/// Synchronous liquid stake. The asset may be native ETH. The protocol may be the receipt contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Stake {
    pub protocol: Address,
    pub asset: Token,
    pub receipt: Token,
}

impl Stake {
    pub fn new(protocol: Address, asset: Token, receipt: Token) -> Result<Self, ActionError> {
        let (protocol, asset, receipt) = staking(protocol, asset, receipt)?;
        Ok(Stake {
            protocol,
            asset,
            receipt,
        })
    }
}

/// Synchronous liquid unstake. The receipt pays the asset in the same transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Unstake {
    pub protocol: Address,
    pub asset: Token,
    pub receipt: Token,
}

impl Unstake {
    pub fn new(protocol: Address, asset: Token, receipt: Token) -> Result<Self, ActionError> {
        let (protocol, asset, receipt) = staking(protocol, asset, receipt)?;
        Ok(Unstake {
            protocol,
            asset,
            receipt,
        })
    }
}

/// Peg convert or token migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Convert {
    pub converter: Address,
    pub token_in: Token,
    pub token_out: Token,
    pub mode: ConvertMode,
}

impl Convert {
    pub fn new(
        converter: Address,
        token_in: Token,
        token_out: Token,
        mode: ConvertMode,
    ) -> Result<Self, ActionError> {
        let converter = checked_address(converter)?;
        erc20(token_in)?;
        erc20(token_out)?;
        different(token_in, token_out)?;
        Ok(Convert {
            converter,
            token_in,
            token_out,
            mode,
        })
    }
}

/// Settlement of one order. A side may name several tokens, and a side may include native ETH.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fill {
    pub settlement: Address,
    pub spend: Vec<Token>,
    pub pay: Vec<Token>,
    pub order: [u8; 32],
}

impl Fill {
    pub fn new(
        settlement: Address,
        spend: Vec<Token>,
        pay: Vec<Token>,
        order: [u8; 32],
    ) -> Result<Self, ActionError> {
        let settlement = checked_address(settlement)?;
        let spend = canon(spend)?;
        let pay = canon(pay)?;
        disjoint(&spend, &pay)?;
        Ok(Fill {
            settlement,
            spend,
            pay,
            order,
        })
    }
}

/// Split one ERC-20 into at least two claims. Principal and yield minted from SY is this.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Split {
    pub protocol: Address,
    pub input: Token,
    pub outputs: Vec<Token>,
}

impl Split {
    pub fn new(protocol: Address, input: Token, outputs: Vec<Token>) -> Result<Self, ActionError> {
        let (protocol, outputs, input) = claims(protocol, outputs, input)?;
        Ok(Split {
            protocol,
            input,
            outputs,
        })
    }
}

/// Merge at least two claims into one ERC-20. Principal and yield redeemed to SY is this.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Merge {
    pub protocol: Address,
    pub inputs: Vec<Token>,
    pub output: Token,
}

impl Merge {
    pub fn new(protocol: Address, inputs: Vec<Token>, output: Token) -> Result<Self, ActionError> {
        let (protocol, inputs, output) = claims(protocol, inputs, output)?;
        Ok(Merge {
            protocol,
            inputs,
            output,
        })
    }
}

/// One synchronous transform.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Wrap(Wrap),
    Unwrap(Unwrap),
    RateWrap(RateWrap),
    RateUnwrap(RateUnwrap),
    Swap(Swap),
    MintLiquidity(MintLiquidity),
    BurnLiquidity(BurnLiquidity),
    DepositVault(DepositVault),
    RedeemVault(RedeemVault),
    Supply(Supply),
    Withdraw(Withdraw),
    Stake(Stake),
    Unstake(Unstake),
    Convert(Convert),
    Fill(Fill),
    Split(Split),
    Merge(Merge),
}

impl Action {
    /// Tokens this action takes. A list is in address order.
    #[must_use]
    pub fn spends(&self) -> Vec<Token> {
        match self {
            Action::Wrap(action) => vec![action.underlying],
            Action::Unwrap(action) => vec![action.wrapper],
            Action::RateWrap(action) => vec![action.underlying],
            Action::RateUnwrap(action) => vec![action.wrapper],
            Action::Swap(action) => vec![action.token_in],
            Action::MintLiquidity(action) => action.tokens.clone(),
            Action::BurnLiquidity(action) => vec![action.share],
            Action::DepositVault(action) => vec![action.asset],
            Action::RedeemVault(action) => vec![action.vault],
            Action::Supply(action) => vec![action.asset],
            Action::Withdraw(action) => vec![action.receipt],
            Action::Stake(action) => vec![action.asset],
            Action::Unstake(action) => vec![action.receipt],
            Action::Convert(action) => vec![action.token_in],
            Action::Fill(action) => action.spend.clone(),
            Action::Split(action) => vec![action.input],
            Action::Merge(action) => action.inputs.clone(),
        }
    }

    /// Tokens this action produces. A list is in address order.
    #[must_use]
    pub fn pays(&self) -> Vec<Token> {
        match self {
            Action::Wrap(action) => vec![action.wrapper],
            Action::Unwrap(action) => vec![action.underlying],
            Action::RateWrap(action) => vec![action.wrapper],
            Action::RateUnwrap(action) => vec![action.underlying],
            Action::Swap(action) => vec![action.token_out],
            Action::MintLiquidity(action) => vec![action.share],
            Action::BurnLiquidity(action) => action.tokens.clone(),
            Action::DepositVault(action) => vec![action.vault],
            Action::RedeemVault(action) => vec![action.asset],
            Action::Supply(action) => vec![action.receipt],
            Action::Withdraw(action) => vec![action.asset],
            Action::Stake(action) => vec![action.receipt],
            Action::Unstake(action) => vec![action.asset],
            Action::Convert(action) => vec![action.token_out],
            Action::Fill(action) => action.pay.clone(),
            Action::Split(action) => action.outputs.clone(),
            Action::Merge(action) => vec![action.output],
        }
    }
}

fn checked_address(address: Address) -> Result<Address, ActionError> {
    if address.0 == [0; 20] {
        return Err(ActionError::ZeroContract);
    }
    Ok(address)
}

fn checked_pool(pool: [u8; 32]) -> Result<[u8; 32], ActionError> {
    if pool == [0; 32] {
        return Err(ActionError::ZeroPool);
    }
    Ok(pool)
}

fn different(left: Token, right: Token) -> Result<(), ActionError> {
    if left == right {
        return Err(ActionError::SameToken);
    }
    Ok(())
}

fn apart(contract: Address, token: Token) -> Result<(), ActionError> {
    if contract.0 == token.0 {
        return Err(ActionError::SameToken);
    }
    Ok(())
}

fn erc20(token: Token) -> Result<(), ActionError> {
    if token == NATIVE {
        return Err(ActionError::NativeToken);
    }
    Ok(())
}

fn erc20s(tokens: &[Token]) -> Result<(), ActionError> {
    for token in tokens {
        erc20(*token)?;
    }
    Ok(())
}

fn canon(mut tokens: Vec<Token>) -> Result<Vec<Token>, ActionError> {
    if tokens.is_empty() {
        return Err(ActionError::EmptySide);
    }
    tokens.sort();
    if tokens.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ActionError::DuplicateToken);
    }
    Ok(tokens)
}

fn disjoint(left: &[Token], right: &[Token]) -> Result<(), ActionError> {
    if left.iter().any(|token| right.contains(token)) {
        return Err(ActionError::SameToken);
    }
    Ok(())
}

fn custody(wrapper: Token, underlying: Token) -> Result<(Token, Token), ActionError> {
    erc20(wrapper)?;
    different(wrapper, underlying)?;
    Ok((wrapper, underlying))
}

fn rate(wrapper: Token, underlying: Token) -> Result<(Token, Token), ActionError> {
    erc20(wrapper)?;
    erc20(underlying)?;
    different(wrapper, underlying)?;
    Ok((wrapper, underlying))
}

fn vault_pair(vault: Token, asset: Token) -> Result<(Token, Token), ActionError> {
    erc20(vault)?;
    erc20(asset)?;
    different(vault, asset)?;
    Ok((vault, asset))
}

fn lending(
    market: Address,
    asset: Token,
    receipt: Token,
) -> Result<(Address, Token, Token), ActionError> {
    let market = checked_address(market)?;
    erc20(asset)?;
    erc20(receipt)?;
    different(asset, receipt)?;
    apart(market, asset)?;
    apart(market, receipt)?;
    Ok((market, asset, receipt))
}

fn staking(
    protocol: Address,
    asset: Token,
    receipt: Token,
) -> Result<(Address, Token, Token), ActionError> {
    let protocol = checked_address(protocol)?;
    erc20(receipt)?;
    different(asset, receipt)?;
    apart(protocol, asset)?;
    Ok((protocol, asset, receipt))
}

struct ShareParts {
    contract: Address,
    pool: [u8; 32],
    share: Token,
    tokens: Vec<Token>,
    shape: ShareShape,
}

fn share_parts(
    contract: Address,
    pool: [u8; 32],
    share: Token,
    tokens: Vec<Token>,
    shape: ShareShape,
) -> Result<ShareParts, ActionError> {
    let contract = checked_address(contract)?;
    let pool = checked_pool(pool)?;
    erc20(share)?;
    let tokens = canon(tokens)?;
    erc20s(&tokens)?;
    if tokens.contains(&share) {
        return Err(ActionError::ShareInPool);
    }
    let count_ok = match shape {
        ShareShape::Single => tokens.len() == 1,
        ShareShape::Proportional | ShareShape::Unbalanced => tokens.len() >= 2,
    };
    if !count_ok {
        return Err(ActionError::Count);
    }
    Ok(ShareParts {
        contract,
        pool,
        share,
        tokens,
        shape,
    })
}

fn claims(
    protocol: Address,
    many: Vec<Token>,
    one: Token,
) -> Result<(Address, Vec<Token>, Token), ActionError> {
    let protocol = checked_address(protocol)?;
    erc20(one)?;
    let many = canon(many)?;
    erc20s(&many)?;
    if many.len() < 2 {
        return Err(ActionError::Count);
    }
    if many.contains(&one) {
        return Err(ActionError::SameToken);
    }
    Ok((protocol, many, one))
}
