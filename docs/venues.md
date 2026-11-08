# Venues in use

Which Ethereum mainnet venues are actually trading, and how to list their pools. Prefer an on-chain view that returns the set. Use a creation event when the contract keeps no list. Scan transactions only when a pool was deployed with neither a list nor a log. Use HTTP when the venue is an off-chain order book.

Checked 2026-11-07. Two live sources, plus `eth_getCode` / `eth_call` where an address is given.

`ratatoskr-venues` performs the enumeration. A list view (`allPairs`, `allPools`, `pool_list`, `idxToPool`) is one `eth_call` of `contracts/lens/IndexLens.sol` creation code with no `to`; the constructor reverts the slice. That call is plain JSON-RPC. Factories that keep no list are found by testing logs blooms in the local header chain. The hit list stays in `discovery.redb`. Identities stay in `entities.redb`. Receipts come from era1 before the merge and from the peer daemon after it, and those payloads stay in redb. CoW and DexScreener stay orientation and are not inputs.

- One CoW production auction, id `14196246`, fetched from `https://solver-instances.s3.eu-central-1.amazonaws.com/prod/mainnet/auction/14196246.json`. Gzip JSON. 5,129 orders, 849 tokens, 1,965 liquidity entries. That file is the body CoW sent its solvers, so the liquidity array is the pool set that auction could route through.
- DexScreener `GET /token-pairs/v1/ethereum/{token}` for WETH, USDC, USDT, DAI, WBTC, and PEPE. The endpoint returns at most 30 pairs per token. It is a ranking of size, not a census.

GeckoTerminal `GET /api/v2/networks/eth/dexes` lists 82 brand names. A brand that appears in neither source above is not a place to start.

## CoW order history

CoW is an off-chain order book that settles in batches. The orders are not a pool list, and they are not recovered from receipts.

The auction file is the history endpoint:

```
https://solver-instances.s3.eu-central-1.amazonaws.com/prod/mainnet/auction/{auctionId}.json
```

Each object is gzip JSON with `id`, `orders`, `tokens`, `liquidity`, `effectiveGasPrice`, and `deadline`. An order has `uid`, `owner`, `sellToken`, `buyToken`, `sellAmount`, `buyAmount`, `kind`, `class`, `validTo`, `partiallyFillable`, and `signature`. `liquidity` is the pool set below.

S3 sends `x-amz-expiration`. On this date the rule was about 30 days: auction `14000000` (2026-10-11) was still there, auction `13500000` was 404. This is the recent order book, not the archive back to the first batch.

`GET https://api.cow.fi/mainnet/api/v2/solver_competition/{auctionId}` is a second HTTP index. It returns the auction id, the deadline block, the winning transaction hashes, and order UIDs with executed amounts. Auction `1000` (block 15,181,149) and auction `10000000` still answered. Auction `14000000` returned 404, so the id space has holes. A UID from that response hydrates with `GET /api/v1/orders/{uid}`. `GET /api/v2/trades` is not a dump: it returns 400 `InvalidTradeFilter` unless the query has exactly one of `owner` or `orderUid`.

## Pools inside that CoW auction

`liquidity[].kind` on auction `14196246`:

| Count | Kind | How the entry is addressed | Enumerate the set |
| --- | --- | --- | --- |
| 809 | `concentratedLiquidity` | Pool address. Router `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45` (Uniswap SwapRouter02). | Uniswap v3 factory has `getPool(tokenA, tokenB, fee)` and no array of every pool. Full set is `PoolCreated` on `0x1F98431c8aD98523631AE4a59f267346ea31F984`. |
| 723 | `constantProduct` | Pair address. Router `0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D` (Uniswap v2). | `allPairsLength` and `allPairs(i)` on factory `0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f`. |
| 232 | `constantProduct` | Pair address. Router `0xd9e1cE17f2641f24aE83637ab66a2cca9C378B9F` (SushiSwap). | Same two views on `0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac`. |
| 50 | `constantProduct` | Pair address. Router `0xEfF92A263d31888d860bD50809A8D171709b7b1c` (PancakeSwap v2). | Same two views on `0x1097053Fd2ea711dad45caCcc45EfF7548fCB362`. |
| 9 | `constantProduct` | Pair address. Router `0xb9960d9bca016e9748be75dd52f02188b9d0829f`. | Read `factory()` on one pair, then `allPairsLength` if that factory is a v2 fork. |
| 121 | `weightedProduct` | Balancer v2 pool id (`balancerPoolId`) and pool address. | Vault `0xBA12222222228d8Ba445958a75a0704d566BF2C8` has `getPoolTokens(poolId)` and no pool array. The set is `PoolRegistered` plus `TokensRegistered`. |
| 21 | `stable` | Same vault, stable pool id. | Same events. |

`PairCreated(address,address,address,uint256)` is the creation log for the constant-product factories, and it matches what `allPairs` returns. The view is enough. The log matters for a fork whose factory has no array, and for telling forks apart: the signature is shared, the emitter is not.

## Large venues that auction did not attach

DexScreener's top-30 sample, same six tokens, same day. Summed 24h volume inside the sample: Uniswap v4 about $163M, Uniswap v3 about $98M, Metric about $62M, Aero Slipstream about $44M, Curve about $13M across labels, then Pancake v3 and Sushi v3. v3, Sushi, and Pancake v2 already appear above. These do not:

**Uniswap v4.** No pool contract and no list function. PoolManager `0x000000000004444c5dc75cB358380D2e3dE08A90`. The set is `Initialize(bytes32 indexed id, address indexed currency0, address indexed currency1, uint24 fee, int24 tickSpacing, address hooks, uint160 sqrtPriceX96, int24 tick)`. The id is the pool. The hook address is part of the key. Fee `0x800000` means the hook sets the fee.

**Metric.** Oracle-bin pools, not constant product. Current factory `0x2a53833cc95548cf52c7b159110e22D3a9018f32` (`MetricOmmPoolFactory`). `nextPoolIdx` and `idxToPool(i)` are on the factory, so the current generation is a view. Docs say older factories are still deployed and are not this address. `PoolCreated` carries the pool, both tokens, the price provider, and the extensions. USDC/USDT pool `0xaef2ad7aBB651945e6D24276715f25BDED0ad41e` was created 2026-10-15. State is bins through data provider `0x9BB15FE2B214E37d19A6c59eE7B132e787FdfB2a`.

**Aero Slipstream.** USDC/USDT 0.002% pool `0x2D90ebFfC87BBE29E4d78e7627adFE9D0f3D7978` (created 2026-10-22) returns `factory()` `0xf4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2`. DexScreener dex id `aero`. Read that factory for a pool array before scanning its creation logs.

**Curve.** Stableswap-NG factory `0x6A8cbed756804B16E05E741eDaBd5cB544AE21bf` and the legacy metapool factory `0xB9fC157394Af804a3578134A6585C0dc9cc990d4` both expose `pool_count` and `pool_list(i)`. Use those. `PlainPoolDeployed` and `MetaPoolDeployed` carry coins, amplification, and fee, and do not carry the pool address. Twocrypto and tricrypto have their own factories. DexScreener labels those rows `two` and `tri`, and stableswap rows `stbl`. Its `pairAddress` for the 3pool is `0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7` plus the two token addresses, not a single field. 3pool itself was a DAO deployment with no factory log. The address provider `0x0000000022D53366457F9d5E68Ec105046FC4383` is the documented way into the registries that still list those pools. A metapool's second coin is the base pool's LP token.

**PancakeSwap v3 and SushiSwap v3.** Same `PoolCreated(address,address,uint24,int24,address)` as Uniswap v3, different factories: Pancake `0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865`, Sushi `0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F`. No pool array. `getPool` needs the tokens and the fee already.

**Balancer v3.** Vault `0xbA1333333333a1BA1108E8412f11850A5C319bA9`. `PoolRegistered` includes the pool, the factory, and the token list. The v2 vault events do not decode as this.

## Seen, not sized

DexScreener also returned these `dexId`s inside the same 30-pair cap, at much smaller volume: Supernova, raw factory addresses `0x956920F97690Bdd76a6a10C7ba931DEC14f61169`, `0x88695A239A0A3F70729C4CB30fd32F9b4978513D`, `0x818339b4E536E707f14980219037c5046b049dD4`, and `0x7de800467aFcE442019884f51A4A1B9143a34fAc`. The first three answer `factory()` on the pair DexScreener listed. `0x88695A23…` is a non-canonical `PoolCreated` emitter. Identify the factory from the pair, then look for `allPairs` or `pool_list` before treating the creation log as the index.

Kyber Elastic, Trader Joe Liquidity Book, Maverick v2, DODO PMM, Carbon, Fluid, and Ekubo are on the GeckoTerminal brand list and were absent from both the CoW liquidity array and this DexScreener sample. Each has its own creation event. The Uniswap v3 decoder will not read them. Pick one up when it shows volume.
