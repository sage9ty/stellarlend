# Strategy registry for yield-routing adapters (`strategy`)

> Issue #1580 — *Build strategy registry for yield-routing adapters.*

## The problem

Idle protocol liquidity can earn yield only if there is a single, audited place
that records which yield adapters exist, how much may be deployed to each, and
how much actually is. Without it, yield routing is ad-hoc and unverifiable —
operators cannot answer "what is allocated where", and there is no
access-controlled path to add or retire an adapter safely.

## The solution

The registry is the source of truth for *intent*; the movement of tokens into
and out of an adapter is settled by a keeper after the registry has authorized
it:

1. **Register** — an admin adds an adapter (`register_strategy`) with a name, a
   hard per-strategy cap (`max_allocation`), and an initial `Active` status.
2. **Target** — the admin sets a `target_allocation` per strategy (≤ its cap).
3. **Route** — a keeper calls `route_strategies`, which compares each active
   strategy's `current_allocation` to its target and emits a typed
   `RouteInstruction` plan (deploy X / withdraw Y) plus a `StrategiesRoutedEvent`.
   The plan is returned to the caller and published for off-chain indexers.
4. **Settle** — once the keeper has actually moved tokens to/from the adapter, it
   calls `deploy_to_strategy` (funds in) or `withdraw_from_strategy` (funds out)
   to record the movement. Both enforce the cap and the target, so the registry
   can never authorize more than the configured bounds.
5. **Retire** — the admin sets a strategy `Deprecated` (no new deploys) or
   removes it once both its current and target allocation are zero.

This keeps the registry self-contained, fully testable, and free of
adapter-specific assumptions: any adapter that respects the deposit/withdraw
accounting works, because the registry never calls into the adapter itself —
it only records authorized allocations.

## Public entrypoints

| Entrypoint | Caller | Purpose |
| --- | --- | --- |
| `register_strategy(caller, adapter, name, max_allocation)` | admin | Register a yield adapter; returns its id |
| `update_strategy(caller, id, max_allocation?, status?)` | admin | Change a strategy's cap and/or status |
| `remove_strategy(caller, id)` | admin | Remove a strategy once it is fully drained (`current == 0`, `target == 0`) |
| `set_strategy_target_allocation(caller, id, target)` | admin | Set the desired allocation (≤ cap; not for `Deprecated`) |
| `route_strategies(caller)` | keeper | Compute and publish the deploy/withdraw plan for all active strategies |
| `deploy_to_strategy(caller, id, amount)` | keeper | Record funds moved into an adapter (≤ cap, ≤ target) |
| `withdraw_from_strategy(caller, id, amount)` | keeper | Record funds moved out of an adapter (≤ current) |
| `set_strategy_registry_config(caller, config)` | admin | Configure the registry (capacity, allocation bounds) |
| `get_strategy_registry_config()` | view | Read the registry config |
| `get_strategy(id)` | view | Read a single strategy |
| `list_strategies()` | view | All non-removed strategies, by id |
| `get_strategy_registry_stats()` | view | Derived registry statistics |
| `get_strategy_target_allocations()` | view | Target allocation per strategy id |

## Strategy lifecycle

```
                   register ──▶ Active
                                 │
        update(status=Inactive) ──┤── update(status=Active) ──▶ Active
                                 ▼
                              Inactive  ──update(status=Deprecated)──▶ Deprecated
                                                                   (terminal: cannot reactivate)
  any status ──remove (current==0 && target==0)──▶ (deleted)
```

- `Active` accepts new deployments and withdrawals.
- `Inactive` blocks new deployments; withdrawals still allowed.
- `Deprecated` is terminal — it cannot be reactivated, and blocks new
  deployments, but withdrawals are still allowed so funds can always be
  retrieved.

## Invariants enforced

- `max_allocation` is a hard ceiling: `deploy` rejects `new_current > cap`
  with `LimitExceeded` (and raises a `Critical` alert).
- `deploy` rejects `new_current > target` with `InvalidState` (you may not
  deploy beyond what was routed).
- Lowering `max_allocation` below the current allocation **or** the target is
  rejected (the cap must stay reachable).
- `remove_strategy` is only allowed when both `current_allocation` and
  `target_allocation` are zero.

## Observability

Every mutation emits a typed event: `StrategyRegisteredEvent`,
`StrategyUpdatedEvent`, `StrategyRemovedEvent`, `StrategyTargetSetEvent`,
`StrategiesRoutedEvent`, `StrategyDeployedEvent`, `StrategyWithdrawnEvent`,
`StrategyRegistryConfiguredEvent`. A cap breach raises a `StrategyAlertEvent`
(severity `Critical`) so off-chain monitors can flag over-allocations.
`RegistryStats` (total/active/inactive/deprecated counts and aggregate
target/current allocations) is derived from the strategy table so it cannot
drift.

## Constants (defaults, overridable via registry config)

| Constant | Default |
| --- | --- |
| `DEFAULT_MAX_STRATEGIES` | 50 |
| `DEFAULT_MAX_ALLOCATION` | `i128::MAX` (effectively uncapped) |
| `DEFAULT_MIN_ALLOCATION` | 0 |

## Testing

Unit tests live in the `scratch-tests` crate
(`stellar-lend/scratch-tests/src/strategy_tests.rs`) and cover: registration and
listing, admin authorization, bad-cap and capacity rejection, target bounds,
target + routing (deploy surplus), deploy/withdraw accounting, cap enforcement
(`LimitExceeded`), the inactive/deprecated status transitions (and that
`Deprecated` is terminal but still allows withdrawals), cap-lowering
validation, removal gating on a cleared target, route skipping of inactive and
zero-delta strategies, route emitting a withdraw when current exceeds target,
the target-allocations view, and registry-config validation.

> The tests are kept in a separate crate (not inline `#[cfg(test)]` modules)
> because `cargo test -p hello-world` is currently blocked by the pre-existing
> `stellarlend-lending` dev-dependency breakage. `scratch-tests` depends on
> `hello-world` as a normal library (so that dev-dependency is never compiled)
> and runs the feature module's public API inside `env.as_contract` frames.
> Run them with `cargo test -p scratch-tests` from `stellar-lend/`.
