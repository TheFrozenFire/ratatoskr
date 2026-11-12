# Problem

Ratatoskr starts from raw chain data and produces objects a searcher can quote without enumerating venues. The search is from one token to another. The expensive part of a naive search is combinatorial quoting. The objects below are the precomputation that removes that work, and the questions that precomputation does not yet answer.

A venue here is a pool in the catalog. A route is a sequence of trades from an input token to an output token. A price curve is the output a quote assigns across input sizes, in the two tokens' own units. A larger input returns more output, and each further unit returns less. That fall in marginal output is the diminishing return of the quote.

## Raw data

Ingestion stores the pool details a quote needs. From those details a simulator can price a trade through that venue. The price curve of one venue is determined by that data, so it can be computed before any search.

## Naive search

A naive search looks for a raw route between two tokens. It starts at one token and enumerates venues that trade the input for the output. It also works outward: token A to token C, then token C to token B, and further compositions of that kind. Each candidate is quoted. The search keeps the candidate that returns more of the output token.

That enumeration is combinatorial. The same input can also be split across several venues. At larger sizes that split often pays more than giving the whole input to the single best venue. A search that quotes each route as a whole, and each venue as a separate routing entity, repeats the split for every candidate.

## What the search actually needs

The search wants actions that take one token to another. A venue is one such action. Several venues between the same two tokens are interchangeable inputs to one allocation: the next unit of input goes to whichever action still pays the most for it.

Two precomputations follow.

The first is a price curve for each venue. Quoting that venue during a search reads the curve.

The second collects every direct action between one ordered pair of tokens and allocates an input across them in advance. That object is a pair conduit.

## Route-finding and the optimizer

Route-finding enumerates the routes that enter the optimization problem. The size of that set is the size of the problem. An optimizer given every route with any capacity between the two tokens would allocate the flow across them. Finding the routes is the slow part. The searcher spends its time on database round-trips, index lookups, and the other costs of planning. Once the set is in hand, the allocation is a small fraction of that time.

The set does not have to contain every route. It has to carry enough capacity, with some margin. The margin is room for shared depletion and for other capacity the search cannot see. The search does not account for shared depletion. It cannot, and that limit is accepted. The broad objective of the set is capacity.

## Pair conduit

A pair conduit is named by two tokens, in order. In this model its members are the direct venues between those tokens. A multi-hop route is not a member. Whether any multi-hop route belongs inside a pair conduit is open.

The allocation orders the marginal pieces of the member curves. The next unit of input goes to the remaining piece with the highest slope, whichever venue that piece belongs to. A venue receives more input later when one of its later pieces is still the steepest piece remaining. For each input size the result is the output of that allocation.

That output is the best these direct venues can produce together when each action's marginal falls as its own input grows, and the actions do not share a pool. When a later piece of a venue curve is steeper than an earlier piece, the curve that enters the allocation is the left-greedy minorant: the prefix whose slopes already fall, with each later piece lowered until its slope no longer rises. Once a slope rises, no increasing concave curve under the samples dominates the others.

That allocation is itself a price curve, the whole-conduit curve. A later quote reads the conduit curve. It does not quote the member venues again. The conduit is the routing entity the search sees for that ordered pair.

A conduit named by more than two tokens is not part of this model. Whether one is useful is open.

## Native ETH

Native ETH and wrapped ETH are different tokens. Native ETH is named by the zero address, because it has no contract. The wrap takes one native ETH and returns one wrapped ETH, at any size. That action is not a venue. A search from native ETH to another asset uses the wrap, then the venues that trade wrapped ETH.

## Hub

A hub is a token the searcher can quote through a cheap lookup. Wrapped ETH and the popular stablecoins are the obvious examples. Hubs also connect deeply to each other. Which tokens qualify is open. Connectivity to many tokens, depth on those connections, and a mix of the two are the candidates.

The stored object is the price curve from a token to the hub, and from the hub onward, including from one hub to another. Which hub returns more output changes with the input size, so a single score for the token does not record the quote. Adding another stored curve never lowers the best output the set can reach. A chain of three or more hubs is the two-hub quote applied again, when the quote from one hub to the next is their direct conduit.

The searcher includes that curve when the lookup is cheap, including when the optimizer later leaves the curve unused. Discovering a route that does not go through a hub costs more than reading the hub curve, so the cost of including the hub curve is the cost of the read.

## What a searcher still composes

After these objects exist, a search from token A to token B can read the pair conduit for that ordered pair. It can also read the precomputed quotes through hubs: A to a hub, that hub to B, and one hub to another hub when the path uses two of them.

A route that is neither the direct pair nor a path through a hub is outside this model. Whether any such route is worth keeping is open. Other fundamental route types may exist, and this model does not name them.
