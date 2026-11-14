# Quotes

For a session that measures or diagnoses quotes. The fit stage is [ARCHITECTURE.md](../ARCHITECTURE.md). The words are [CONTEXT.md](../CONTEXT.md).

## Three amounts

A stored price curve, a local quote, and a chain quote are three amounts.

The optimizer pays a stored price curve through `routing::quote` on slope segments in `crates/routing/src/lib.rs`. The searcher returns those segments. A local quote is `PoolDetail::quote` in `crates/venues/src/detail/mod.rs`, computed from the quote inputs. A chain quote is an execution through QuoteLens.

Output that matches the stored segments does not mean the local quote matches the chain quote. A change to local-quote code does not change a stored plan until that kind's curves are written again and the pair conduits those curves feed are rebuilt. The state pipeline's fit stage writes the curve. `ratatoskr --refresh-kinds` re-reads the named kinds through the detail lens at one chain head, stores each curve, and drains the conduit jobs that write queued. A concentrated pool whose tick book is not kept stores no curve.

## Chain quote

The call target is QuoteLens at `0x0000000000000000000000000000000000510e51`, `contracts/lens/QuoteLens.sol`. `prepare` and `dispatch_raw` in `crates/venues/src/fit.rs` build that call. The stored method name is `"swap"`. `dispatch_raw` sends the calls inside Multicall3. `eth_call` uses the quoting state's block and a state override that installs the lens runtime at that address. Block 0 is refused before any RPC. `eth_call_at` in `crates/venues/src/rpc.rs` still accepts a missing block as `latest`. Quote calls use `eth_call_block`, which requires a block.

The lens returns the output-token balance delta. An input tax is what the pool received. An output tax is what the caller kept. Quote addresses in `corpus/mainnet.yaml` stay configuration on the enumeration root. They are not the call target. `drive_quotes` is not a pipeline stage. The lens does not change `PoolDetail::quote`, selection, or stored curves.

A failed chain quote is an execution result. It is not a reason to change the fitter. The stored text is the revert string, or `"reverted"` with a short payload prefix. A v4 pool manager can revert with `PoolNotInitialized` or `CurrencyNotSettled`. The fitter does not decode those names. A missing balance slot is stored as `"balance slot"` before the call. A token that refuses the lens transfer is stored as `"transfer"`. A native-coin Curve pool is skipped as `"native curve"`. A failed chain quote is not, by itself, a tax measurement. Tax detection is not done here, and a wrong on-chain `decimals()` is not corrected. Do not hardcode a decimals map. Do not drop steep slope segments to hide a disagreement.

## Local quote

The fitter is scored against the local quote.

Curve crypto goes through `quote_crypto`, which calls `crypto2_out` or `crypto3_out`. Both take the coin balances, the precisions, amplification (`pool.amp` as the `ann` argument), gamma, `D`, the two indexes, the input, and the mid fee, out fee, and fee gamma. The two-coin price scale is `coins[0].price_scale` (`scale`): coin 1 is brought in as that scale over `10^18` (`WAD`, a `BigInt`). The three-coin scales are `coins[0].price_scale` and `coins[1].price_scale`, applied the same way to coins 1 and 2. Amplification is a `U256`. Zero, or a value above `1_000_000_000_000` (`10^12`, the amplification bound), returns `"amp"`. A coin whose `precision` (`U256`, role `10^(18-decimals)`) is zero returns `"precision"`. A coin count other than 2 or 3 returns `"coins"`. A stableswap stays on `stable_out` when gamma is zero and the crypto flag is unset. That path's fee denominator is the `U256` `10_000_000_000` (`10^10`).

Balancer v3 `getCurrentLiveBalances` is already raw times decimal scaling times the token rate, over `1e18`. The local quote unscales that back to raw. Multiplying by the rate again inflates the quote. Kind 11 (Balancer v2) leaves balances raw. A stableswap whose invariant solver returns `"invariant"` produces no output amount.

A Metric oracle price is whole token1 per whole token0. `quote_metric` reads `marginal_price_x64` as that price in Q64 (`U256`, `1 << 64`). Selling token0 multiplies the input by the price and divides by `2^64`. Selling token1 does the reverse. `scale` is `10^(D - decimals)`. The raw output is that whole-token amount times `scale_in / scale_out` (`scale0` and `scale1` follow which token is sold). An empty bin, scaled output balance `0`, pays nothing.

## One judgment

When a finished plan's numeraire output exceeds its numeraire input on a trade whose honest ceiling is near one-for-one, say so in the same report and name a plausible cause. A chain quote that matches the local quote on a pool does not explain a surplus that comes from using two pools' curves together. Do not treat that surplus as a successful retained value.
