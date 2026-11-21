# Per-pool withdrawal queue (`withdraw_queue`)

> Issue #1586 — *Implement per-pool withdrawal queue when utilization is pinned.*

## The problem

A pool whose utilization is **pinned** — sitting at or above a configurable
cap — has no spare unencumbered liquidity to honour a withdrawal right now.
The legacy withdraw path reverts the user, who then has to retry by hand until
utilization relaxes. Users cannot route around it.

## The solution

A single user-facing entry, `request_withdrawal`, reads the pool's current
utilization and decides:

- **Not pinned** → fulfil immediately through the canonical
  `withdraw::withdraw_collateral` path. The user is debited and paid in one
  transaction.
- **Pinned** → the request is appended to a **per-pool FIFO queue**. The user
  receives a request id and a 1-indexed queue position. No collateral moves yet.

A keeper periodically calls `process_withdraw_queue`, which drains the queue in
FIFO order against the pool's *current* available liquidity, fulfilling each
whole request by invoking `withdraw::withdraw_collateral` on the queued user's
behalf. Requests that still do not fit the available liquidity are skipped
(best-effort FIFO) and remain pending; requests whose fulfilment keeps failing
past `max_retries` are marked `Failed` so the user can cancel and re-submit.
Stale requests expire automatically.

The pinning threshold is **per-pool** (`WithdrawQueueConfig::utilization_pin_threshold_bps`),
so different pools can queue at different utilization levels. The utilization
value is read from the `interest_rate` module.

## Public entrypoints

| Entrypoint | Caller | Purpose |
| --- | --- | --- |
| `request_withdrawal(pool, user, amount)` | user | Withdraw now, or queue when pinned |
| `cancel_withdraw_request(entry_id, caller)` | user / admin | Cancel a pending request |
| `process_withdraw_queue(pool, processor, max_count)` | keeper | Drain the queue in FIFO order |
| `cleanup_withdraw_queue(pool)` | anyone | Mark expired requests expired |
| `set_withdraw_queue_config(caller, pool, config)` | admin | Configure a pool's queue |
| `get_withdraw_queue_config(pool)` | view | Read a pool's config |
| `get_withdraw_request(entry_id)` | view | Read a single request |
| `get_pending_withdraw_requests(pool)` | view | Pending requests in FIFO order |
| `get_withdraw_queue_stats(pool)` | view | Derived per-pool statistics |
| `get_withdraw_queue_alerts(pool, max_count)` | view | Recent backlog alerts |
| `is_pool_utilization_pinned(pool)` | view | Whether a pool is currently pinned |

## Request lifecycle

```
Pending ──process──▶ Processing ──withdraw ok──▶ Fulfilled
   │                     │
   │                     └──withdraw err──▶ (retries < max) ──▶ Pending
   │                                       (retries ≥ max) ──▶ Failed
   ├──cancel──▶ Cancelled
   └──expiry──▶ Expired
```

## Observability

Every state transition emits a typed event: `WithdrawQueuedEvent`,
`WithdrawFulfilledEvent`, `WithdrawCancelledEvent`, `WithdrawExpiredEvent`,
`WithdrawFailedEvent`, `WithdrawQueueConfiguredEvent`. Backlog growth raises a
`WithdrawQueueAlert` (Warning / Critical) with its own `WithdrawQueueAlertEvent`
so off-chain monitors can page before the queue saturates. `WithdrawQueueStats`
(total/pending/fulfilled/cancelled/expired/failed counts and amounts) is
derived from the entry table so it can never drift.

## Constants (defaults, overridable per pool)

| Constant | Default |
| --- | --- |
| `DEFAULT_MAX_QUEUE_SIZE` | 1000 |
| `DEFAULT_PIN_THRESHOLD_BPS` | 9500 (95%) |
| `DEFAULT_ENTRY_EXPIRATION_SECONDS` | 86400 (24h) |
| `DEFAULT_MAX_BATCH_SIZE` | 25 |
| `DEFAULT_MIN_WITHDRAWAL_AMOUNT` | 1 |
| `DEFAULT_MAX_RETRIES` | 3 |

## Testing

Unit tests live in the `scratch-tests` crate (`stellar-lend/scratch-tests/src/withdraw_queue_tests.rs`)
and cover: the pinning threshold, config defaults and validation, admin
authorization, the queue path, the instant path, FIFO ordering and position,
the queue-size limit, cancellation, missing-request handling, processing
with available liquidity, skip-over of over-large requests, the retry-then-fail
path, expiry cleanup, backlog alerts, and stats/views consistency.

> The tests are kept in a separate crate (not inline `#[cfg(test)]` modules)
> because `cargo test -p hello-world` is currently blocked by the pre-existing
> `stellarlend-lending` dev-dependency breakage. `scratch-tests` depends on
> `hello-world` as a normal library (so that dev-dependency is never compiled)
> and runs the feature module's public API inside `env.as_contract` frames.
> Run them with `cargo test -p scratch-tests` from `stellar-lend/`.
