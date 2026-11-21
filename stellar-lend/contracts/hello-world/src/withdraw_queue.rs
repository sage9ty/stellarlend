//! # Per-pool withdrawal queue (#1586)
//!
//! ## Why this exists
//!
//! A pool whose utilization is *pinned* — sitting at or above a configurable
//! cap — has no spare unencumbered liquidity to honour a withdrawal right now.
//! The legacy [`crate::withdraw`] path would simply revert the user, forcing
//! them to retry by hand until utilization relaxes. That is a lossy, risky
//! workaround: users cannot route around it, so the gap is filled here.
//!
//! ## Model
//!
//! [`request_withdrawal`] is the single user-facing entry. It reads the pool's
//! current utilization (via [`crate::interest_rate`]) and decides:
//!
//! - **Not pinned** → fulfil immediately through the canonical
//!   [`crate::withdraw::withdraw_collateral`] path (no new behaviour, just
//!   reuse). The user is debited and paid in one transaction.
//! - **Pinned** → the request is appended to a per-pool FIFO queue and the user
//!   receives a request id plus their queue position. No collateral moves yet.
//!
//! A keeper (any authorized caller) periodically calls [`process_withdrawals`],
//! which drains the queue in FIFO order against the pool's *current* available
//! liquidity, fulfilling each whole request by invoking
//! [`crate::withdraw::withdraw_collateral`] on the queued user's behalf.
//! Requests that still do not fit the available liquidity are skipped (best
//! effort FIFO) and remain pending; requests whose fulfilment keeps failing
//! past [`WithdrawQueueConfig::max_retries`] are marked failed so the user can
//! cancel and re-submit. Stale requests expire automatically.
//!
//! ## Storage layout
//!
//! | Key | Storage | Value |
//! | --- | --- | --- |
//! | `WithdrawQueueKey::Config(pool)` | persistent | [`WithdrawQueueConfig`] |
//! | `WithdrawQueueKey::NextEntryId` | instance | `u64` (global monotonic) |
//! | `WithdrawQueueKey::Entry(id)` | persistent | [`WithdrawQueueEntry`] |
//! | `WithdrawQueueKey::NextAlertId(pool)` | instance | `u64` |
//! | `WithdrawQueueKey::Alert(pool, id)` | persistent | [`WithdrawQueueAlert`] |
//! | `WithdrawQueueKey::AlertsRaised(pool)` | persistent | `u32` |
//!
//! Per-pool stats are derived on demand from the entry table so they can never
//! drift from the on-chain state.
//!
//! ## Observability
//!
//! Every state transition emits a typed event (`WithdrawQueuedEvent`,
//! `WithdrawFulfilledEvent`, `WithdrawCancelledEvent`, `WithdrawExpiredEvent`,
//! `WithdrawFailedEvent`, `WithdrawQueueConfiguredEvent`). Backlog growth
//! raises a [`WithdrawQueueAlert`] (Warning / Critical) with its own event so
//! off-chain monitors can page before the queue saturates.

use soroban_sdk::{contractevent, contracttype, Address, Env, Symbol, Vec};

use crate::errors::LendingError;

// ─── Tunables ───────────────────────────────────────────────────────────────

/// Default maximum number of pending requests per pool before enqueue is rejected.
pub const DEFAULT_MAX_QUEUE_SIZE: u32 = 1_000;
/// Default utilization (in bps) at/above which a pool is considered pinned.
pub const DEFAULT_PIN_THRESHOLD_BPS: i128 = 9_500; // 95%
/// Default lifetime of a queued request before it expires.
pub const DEFAULT_ENTRY_EXPIRATION_SECONDS: u64 = 24 * 60 * 60; // 24h
/// Default maximum requests a single `process_withdrawals` call may fulfil.
pub const DEFAULT_MAX_BATCH_SIZE: u32 = 25;
/// Default minimum request size; guards against dust spam.
pub const DEFAULT_MIN_WITHDRAWAL_AMOUNT: i128 = 1;
/// Default terminal-failure retry cap before a request is marked failed.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Utilization (100%) at/above which a pool is treated as pinned. Tests run
/// at 0% utilization, so a threshold of 10_000 (`util >= 10_000`) is never
/// reached and effectively forces the instant (non-queued) path.
pub const NEVER_PINNED_BPS: i128 = 10_000;

// ─── Storage keys ───────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum WithdrawQueueKey {
    /// Per-pool queue configuration.
    Config(Option<Address>),
    /// Global monotonic entry id counter.
    NextEntryId,
    /// A single queued withdrawal request keyed by its global id.
    Entry(u64),
    /// Per-pool monotonic alert id counter.
    NextAlertId(Option<Address>),
    /// A per-pool backlog alert keyed by alert id.
    Alert(Option<Address>, u64),
    /// Per-pool cumulative count of raised alerts.
    AlertsRaised(Option<Address>),
}

// ─── Types ──────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum WithdrawQueueStatus {
    Pending,
    Processing,
    Fulfilled,
    Cancelled,
    Expired,
    Failed,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct WithdrawQueueEntry {
    pub id: u64,
    pub pool: Option<Address>,
    pub user: Address,
    pub amount: i128,
    pub queued_at: u64,
    pub expires_at: u64,
    pub status: WithdrawQueueStatus,
    pub fulfilled_amount: i128,
    pub retries: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct WithdrawQueueConfig {
    pub max_queue_size: u32,
    pub utilization_pin_threshold_bps: i128,
    pub entry_expiration_seconds: u64,
    pub max_batch_size: u32,
    pub min_withdrawal_amount: i128,
    pub max_retries: u32,
}

impl Default for WithdrawQueueConfig {
    fn default() -> Self {
        Self {
            max_queue_size: DEFAULT_MAX_QUEUE_SIZE,
            utilization_pin_threshold_bps: DEFAULT_PIN_THRESHOLD_BPS,
            entry_expiration_seconds: DEFAULT_ENTRY_EXPIRATION_SECONDS,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            min_withdrawal_amount: DEFAULT_MIN_WITHDRAWAL_AMOUNT,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

/// Result of [`request_withdrawal`]: either fulfilled on the spot or queued.
///
/// Soroban `#[contracttype]` enums cannot carry named fields, so the outcome
/// is a struct discriminated by [`WithdrawRequestOutcome::queued`].
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct WithdrawRequestOutcome {
    /// `true` when the request was queued because the pool is pinned;
    /// `false` when it was fulfilled immediately.
    pub queued: bool,
    /// Request id (valid when `queued`).
    pub entry_id: u64,
    /// 1-indexed FIFO position in the pool's pending set (valid when `queued`).
    pub position: u32,
    /// Amount fulfilled (valid when `!queued`).
    pub fulfilled_amount: i128,
}

/// Aggregate result of [`process_withdrawals`].
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessResult {
    pub fulfilled: u32,
    pub failed: u32,
    pub total_fulfilled_amount: i128,
}

/// Severity of a backlog alert.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum WithdrawAlertSeverity {
    Info,
    Warning,
    Critical,
}

/// A raised backlog alert for a pool.
#[contracttype]
#[derive(Clone, Debug)]
pub struct WithdrawQueueAlert {
    pub alert_id: u64,
    pub pool: Option<Address>,
    pub severity: WithdrawAlertSeverity,
    pub message: Symbol,
    pub timestamp: u64,
    pub queue_size: u32,
}

/// Derived per-pool queue statistics.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct WithdrawQueueStats {
    pub total_requests: u64,
    pub pending: u32,
    pub processing: u32,
    pub fulfilled: u32,
    pub cancelled: u32,
    pub expired: u32,
    pub failed: u32,
    pub total_amount_queued: i128,
    pub total_amount_fulfilled: i128,
    pub alerts_raised: u32,
}

impl WithdrawQueueStats {
    fn zero() -> Self {
        Self {
            total_requests: 0,
            pending: 0,
            processing: 0,
            fulfilled: 0,
            cancelled: 0,
            expired: 0,
            failed: 0,
            total_amount_queued: 0,
            total_amount_fulfilled: 0,
            alerts_raised: 0,
        }
    }
}

// ─── Events ──────────────────────────────────────────────────────────────────

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawQueuedEvent {
    #[topic]
    pub user: Address,
    pub entry_id: u64,
    pub pool: Option<Address>,
    pub amount: i128,
    pub position: u32,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawFulfilledEvent {
    #[topic]
    pub user: Address,
    pub entry_id: u64,
    pub pool: Option<Address>,
    pub amount: i128,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawCancelledEvent {
    #[topic]
    pub user: Address,
    pub entry_id: u64,
    pub pool: Option<Address>,
    pub caller: Address,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawExpiredEvent {
    pub entry_id: u64,
    pub pool: Option<Address>,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawFailedEvent {
    #[topic]
    pub user: Address,
    pub entry_id: u64,
    pub pool: Option<Address>,
    pub retries: u32,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawQueueConfiguredEvent {
    pub pool: Option<Address>,
    pub caller: Address,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct WithdrawQueueAlertEvent {
    pub alert_id: u64,
    pub pool: Option<Address>,
    pub severity: WithdrawAlertSeverity,
    pub queue_size: u32,
    pub timestamp: u64,
}

// ─── Pure helpers ───────────────────────────────────────────────────────────

/// A pool is pinned when its utilization is at/above the configured threshold.
pub fn is_pinned(utilization_bps: i128, threshold_bps: i128) -> bool {
    utilization_bps >= threshold_bps
}

// ─── Config ──────────────────────────────────────────────────────────────────

pub fn get_config(env: &Env, pool: &Option<Address>) -> WithdrawQueueConfig {
    env.storage()
        .persistent()
        .get::<WithdrawQueueKey, WithdrawQueueConfig>(&WithdrawQueueKey::Config(pool.clone()))
        .unwrap_or_default()
}

/// Admin-only: replace the per-pool queue configuration.
pub fn set_config(
    env: &Env,
    caller: &Address,
    pool: &Option<Address>,
    config: WithdrawQueueConfig,
) -> Result<(), LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;
    validate_config(&config)?;
    env.storage()
        .persistent()
        .set(&WithdrawQueueKey::Config(pool.clone()), &config);
    WithdrawQueueConfiguredEvent {
        pool: pool.clone(),
        caller: caller.clone(),
        timestamp: env.ledger().timestamp(),
    }
    .publish(env);
    Ok(())
}

fn validate_config(config: &WithdrawQueueConfig) -> Result<(), LendingError> {
    if config.max_queue_size == 0 {
        return Err(LendingError::InvalidParameter);
    }
    if !(0..=10_000).contains(&config.utilization_pin_threshold_bps) {
        return Err(LendingError::InvalidParameter);
    }
    if config.entry_expiration_seconds == 0 {
        return Err(LendingError::InvalidParameter);
    }
    if config.max_batch_size == 0 {
        return Err(LendingError::InvalidParameter);
    }
    if config.min_withdrawal_amount < 0 {
        return Err(LendingError::InvalidAmount);
    }
    if config.max_retries == 0 {
        return Err(LendingError::InvalidParameter);
    }
    Ok(())
}

// ─── Pinning view ───────────────────────────────────────────────────────────

/// Whether `pool` is currently pinned.
///
/// The utilization value is read from the interest-rate module; the pinning
/// threshold is the per-pool [`WithdrawQueueConfig::utilization_pin_threshold_bps`],
/// so different pools can pin at different utilization levels.
pub fn is_utilization_pinned(env: &Env, pool: &Option<Address>) -> bool {
    let config = get_config(env, pool);
    let utilization_bps = crate::interest_rate::get_current_utilization(env).unwrap_or(0);
    is_pinned(utilization_bps, config.utilization_pin_threshold_bps)
}

// ─── Request lifecycle ──────────────────────────────────────────────────────

/// Request a withdrawal from `pool` for `user` of `amount`.
///
/// Fulfils instantly when the pool is not pinned; otherwise enqueues the
/// request on the per-pool FIFO queue and returns the request id and position.
pub fn request_withdrawal(
    env: &Env,
    pool: Option<Address>,
    user: Address,
    amount: i128,
) -> Result<WithdrawRequestOutcome, LendingError> {
    user.require_auth();

    let config = get_config(env, &pool);
    if amount <= 0 || amount < config.min_withdrawal_amount {
        return Err(LendingError::InvalidAmount);
    }

    let utilization_bps = crate::interest_rate::get_current_utilization(env).unwrap_or(0);

    if is_pinned(utilization_bps, config.utilization_pin_threshold_bps) {
        // Pinned: defer to the queue.
        let pending = count_pending(env, &pool);
        if pending >= config.max_queue_size {
            return Err(LendingError::LimitExceeded);
        }

        let id = next_entry_id(env);
        let now = env.ledger().timestamp();
        let expires_at = now.saturating_add(config.entry_expiration_seconds);

        let entry = WithdrawQueueEntry {
            id,
            pool: pool.clone(),
            user: user.clone(),
            amount,
            queued_at: now,
            expires_at,
            status: WithdrawQueueStatus::Pending,
            fulfilled_amount: 0,
            retries: 0,
        };
        env.storage()
            .persistent()
            .set(&WithdrawQueueKey::Entry(id), &entry);
        bump_next_entry_id(env);

        let position = pending.saturating_add(1);
        WithdrawQueuedEvent {
            user: user.clone(),
            entry_id: id,
            pool: pool.clone(),
            amount,
            position,
            timestamp: now,
        }
        .publish(env);

        maybe_raise_alert(env, &pool, &config, position);
        Ok(WithdrawRequestOutcome {
            queued: true,
            entry_id: id,
            position,
            fulfilled_amount: 0,
        })
    } else {
        // Not pinned: fulfil now through the canonical withdraw path.
        crate::withdraw::withdraw_collateral(env, user.clone(), pool.clone(), amount)?;
        WithdrawFulfilledEvent {
            user: user.clone(),
            entry_id: 0,
            pool: pool.clone(),
            amount,
            timestamp: env.ledger().timestamp(),
        }
        .publish(env);
        Ok(WithdrawRequestOutcome {
            queued: false,
            entry_id: 0,
            position: 0,
            fulfilled_amount: amount,
        })
    }
}

/// Cancel a queued request. Only the requesting user or an admin may cancel,
/// and only while the request is still pending.
pub fn cancel_request(
    env: &Env,
    entry_id: u64,
    caller: Address,
) -> Result<(), LendingError> {
    caller.require_auth();

    let key = WithdrawQueueKey::Entry(entry_id);
    let mut entry: WithdrawQueueEntry = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(LendingError::DataNotFound)?;

    let is_admin = crate::admin::get_admin(env)
        .map(|admin| admin == caller)
        .unwrap_or(false);
    if !is_admin && entry.user != caller {
        return Err(LendingError::Unauthorized);
    }
    if entry.status != WithdrawQueueStatus::Pending {
        return Err(LendingError::InvalidState);
    }

    entry.status = WithdrawQueueStatus::Cancelled;
    env.storage().persistent().set(&key, &entry);

    WithdrawCancelledEvent {
        user: entry.user.clone(),
        entry_id,
        pool: entry.pool.clone(),
        caller,
        timestamp: env.ledger().timestamp(),
    }
    .publish(env);
    Ok(())
}

/// Drain the per-pool queue in FIFO order against current available liquidity.
///
/// Returns the number of requests fulfilled, the number terminally failed, and
/// the total amount moved. Requests that do not fit the available liquidity are
/// skipped (best-effort FIFO) and remain pending.
pub fn process_withdrawals(
    env: &Env,
    pool: Option<Address>,
    processor: Address,
    max_count: u32,
) -> Result<ProcessResult, LendingError> {
    processor.require_auth();

    let config = get_config(env, &pool);
    cleanup_expired(env, &pool);

    let analytics = crate::deposit::get_protocol_analytics(env);
    let mut remaining = analytics.total_deposits.saturating_sub(analytics.total_borrows);

    let batch = max_count.min(config.max_batch_size);
    let mut fulfilled = 0u32;
    let mut failed = 0u32;
    let mut total_amount = 0i128;
    let mut attempts = 0u32;
    let now = env.ledger().timestamp();

    let next_id = next_entry_id(env);
    for id in 0..next_id {
        if attempts >= batch {
            break;
        }
        let key = WithdrawQueueKey::Entry(id);
        let Some(mut entry) = env
            .storage()
            .persistent()
            .get::<WithdrawQueueKey, WithdrawQueueEntry>(&key)
        else {
            continue;
        };
        if entry.pool != pool || entry.status != WithdrawQueueStatus::Pending {
            continue;
        }
        if entry.amount <= 0 || entry.amount > remaining {
            // Dust or larger than currently available: leave pending.
            continue;
        }

        // Reserve the slot and mark processing.
        entry.status = WithdrawQueueStatus::Processing;
        env.storage().persistent().set(&key, &entry);
        attempts += 1;

        match crate::withdraw::withdraw_collateral(
            env,
            entry.user.clone(),
            pool.clone(),
            entry.amount,
        ) {
            Ok(_) => {
                entry.status = WithdrawQueueStatus::Fulfilled;
                entry.fulfilled_amount = entry.amount;
                env.storage().persistent().set(&key, &entry);
                remaining = remaining.saturating_sub(entry.amount);
                fulfilled += 1;
                total_amount = total_amount.saturating_add(entry.amount);
                WithdrawFulfilledEvent {
                    user: entry.user.clone(),
                    entry_id: id,
                    pool: pool.clone(),
                    amount: entry.amount,
                    timestamp: now,
                }
                .publish(env);
            }
            Err(_) => {
                entry.retries = entry.retries.saturating_add(1);
                if entry.retries >= config.max_retries {
                    entry.status = WithdrawQueueStatus::Failed;
                    env.storage().persistent().set(&key, &entry);
                    failed += 1;
                    WithdrawFailedEvent {
                        user: entry.user.clone(),
                        entry_id: id,
                        pool: pool.clone(),
                        retries: entry.retries,
                        timestamp: now,
                    }
                    .publish(env);
                } else {
                    // Reset for a later retry attempt.
                    entry.status = WithdrawQueueStatus::Pending;
                    env.storage().persistent().set(&key, &entry);
                }
            }
        }
    }

    Ok(ProcessResult {
        fulfilled,
        failed,
        total_fulfilled_amount: total_amount,
    })
}

/// Mark every pending request for `pool` that has passed its expiry as expired.
pub fn cleanup_expired(env: &Env, pool: &Option<Address>) -> u32 {
    let now = env.ledger().timestamp();
    let next_id = next_entry_id(env);
    let mut cleaned = 0u32;

    for id in 0..next_id {
        let key = WithdrawQueueKey::Entry(id);
        if let Some(mut entry) = env
            .storage()
            .persistent()
            .get::<WithdrawQueueKey, WithdrawQueueEntry>(&key)
        {
            if entry.pool == *pool
                && entry.status == WithdrawQueueStatus::Pending
                && now > entry.expires_at
            {
                entry.status = WithdrawQueueStatus::Expired;
                env.storage().persistent().set(&key, &entry);
                cleaned += 1;
                WithdrawExpiredEvent {
                    entry_id: id,
                    pool: pool.clone(),
                    timestamp: now,
                }
                .publish(env);
            }
        }
    }
    cleaned
}

// ─── Views ───────────────────────────────────────────────────────────────────

pub fn get_request(env: &Env, entry_id: u64) -> Option<WithdrawQueueEntry> {
    env.storage()
        .persistent()
        .get(&WithdrawQueueKey::Entry(entry_id))
}

/// Pending requests for `pool` in FIFO (id) order.
pub fn get_pending_requests(env: &Env, pool: &Option<Address>) -> Vec<WithdrawQueueEntry> {
    let next_id = next_entry_id(env);
    let mut out = Vec::new(env);
    for id in 0..next_id {
        if let Some(entry) = env
            .storage()
            .persistent()
            .get::<WithdrawQueueKey, WithdrawQueueEntry>(&WithdrawQueueKey::Entry(id))
        {
            if entry.pool == *pool && entry.status == WithdrawQueueStatus::Pending {
                out.push_back(entry);
            }
        }
    }
    out
}

/// Derived per-pool statistics, computed from the entry table.
pub fn get_stats(env: &Env, pool: &Option<Address>) -> WithdrawQueueStats {
    let next_id = next_entry_id(env);
    let mut stats = WithdrawQueueStats::zero();
    let mut total_for_pool = 0u64;

    for id in 0..next_id {
        if let Some(entry) = env
            .storage()
            .persistent()
            .get::<WithdrawQueueKey, WithdrawQueueEntry>(&WithdrawQueueKey::Entry(id))
        {
            if entry.pool != *pool {
                continue;
            }
            total_for_pool += 1;
            match entry.status {
                WithdrawQueueStatus::Pending => stats.pending += 1,
                WithdrawQueueStatus::Processing => stats.processing += 1,
                WithdrawQueueStatus::Fulfilled => stats.fulfilled += 1,
                WithdrawQueueStatus::Cancelled => stats.cancelled += 1,
                WithdrawQueueStatus::Expired => stats.expired += 1,
                WithdrawQueueStatus::Failed => stats.failed += 1,
            }
            stats.total_amount_queued = stats.total_amount_queued.saturating_add(entry.amount);
            stats.total_amount_fulfilled =
                stats.total_amount_fulfilled.saturating_add(entry.fulfilled_amount);
        }
    }
    stats.total_requests = total_for_pool;
    stats.alerts_raised = env
        .storage()
        .persistent()
        .get::<WithdrawQueueKey, u32>(&WithdrawQueueKey::AlertsRaised(pool.clone()))
        .unwrap_or(0);
    stats
}

/// Recent backlog alerts for `pool`, newest first, capped at `max_count`.
pub fn get_alerts(env: &Env, pool: &Option<Address>, max_count: u32) -> Vec<WithdrawQueueAlert> {
    let next_id: u64 = env
        .storage()
        .instance()
        .get(&WithdrawQueueKey::NextAlertId(pool.clone()))
        .unwrap_or(0);
    let mut out = Vec::new(env);
    let start = if next_id > max_count as u64 {
        next_id - max_count as u64
    } else {
        0
    };
    for id in start..next_id {
        if let Some(alert) = env
            .storage()
            .persistent()
            .get::<WithdrawQueueKey, WithdrawQueueAlert>(&WithdrawQueueKey::Alert(
                pool.clone(),
                id,
            ))
        {
            out.push_back(alert);
        }
    }
    out
}

// ─── Internals ───────────────────────────────────────────────────────────────

fn require_admin(env: &Env, caller: &Address) -> Result<(), LendingError> {
    let admin = crate::admin::get_admin(env).ok_or(LendingError::Unauthorized)?;
    if &admin != caller {
        return Err(LendingError::Unauthorized);
    }
    Ok(())
}

fn next_entry_id(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get::<WithdrawQueueKey, u64>(&WithdrawQueueKey::NextEntryId)
        .unwrap_or(0)
}

fn bump_next_entry_id(env: &Env) {
    let next = next_entry_id(env).saturating_add(1);
    env.storage()
        .instance()
        .set(&WithdrawQueueKey::NextEntryId, &next);
}

fn count_pending(env: &Env, pool: &Option<Address>) -> u32 {
    let next_id = next_entry_id(env);
    let mut count = 0u32;
    for id in 0..next_id {
        if let Some(entry) = env
            .storage()
            .persistent()
            .get::<WithdrawQueueKey, WithdrawQueueEntry>(&WithdrawQueueKey::Entry(id))
        {
            if entry.pool == *pool && entry.status == WithdrawQueueStatus::Pending {
                count += 1;
            }
        }
    }
    count
}

fn next_alert_id(env: &Env, pool: &Option<Address>) -> u64 {
    env.storage()
        .instance()
        .get::<WithdrawQueueKey, u64>(&WithdrawQueueKey::NextAlertId(pool.clone()))
        .unwrap_or(0)
}

fn bump_alert_counter(env: &Env, pool: &Option<Address>) {
    let next = next_alert_id(env, pool).saturating_add(1);
    env.storage()
        .instance()
        .set(&WithdrawQueueKey::NextAlertId(pool.clone()), &next);
    let raised: u32 = env
        .storage()
        .persistent()
        .get::<WithdrawQueueKey, u32>(&WithdrawQueueKey::AlertsRaised(pool.clone()))
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&WithdrawQueueKey::AlertsRaised(pool.clone()), &(raised + 1));
}

fn maybe_raise_alert(
    env: &Env,
    pool: &Option<Address>,
    config: &WithdrawQueueConfig,
    queue_size: u32,
) {
    let half = config.max_queue_size / 2;
    let three_quarters = (config.max_queue_size * 3) / 4;

    let severity = if queue_size >= three_quarters && three_quarters > 0 {
        WithdrawAlertSeverity::Critical
    } else if queue_size > half {
        WithdrawAlertSeverity::Warning
    } else {
        return;
    };

    let message = if severity == WithdrawAlertSeverity::Critical {
        Symbol::new(env, "wq_backlog_critical")
    } else {
        Symbol::new(env, "wq_backlog_high")
    };

    let alert_id = next_alert_id(env, pool);
    let alert = WithdrawQueueAlert {
        alert_id,
        pool: pool.clone(),
        severity: severity.clone(),
        message,
        timestamp: env.ledger().timestamp(),
        queue_size,
    };
    env.storage()
        .persistent()
        .set(&WithdrawQueueKey::Alert(pool.clone(), alert_id), &alert);
    bump_alert_counter(env, pool);

    WithdrawQueueAlertEvent {
        alert_id,
        pool: pool.clone(),
        severity: alert.severity.clone(),
        queue_size,
        timestamp: alert.timestamp,
    }
    .publish(env);
}
// Tests for this module live in the `scratch-tests` crate at
// `scratch-tests/src/withdraw_queue_tests.rs`. They are kept out of the inline
// `#[cfg(test)]` path because `cargo test -p hello-world` is currently
// blocked by the pre-existing `stellarlend-lending` dev-dependency
// breakage; the separate crate exercises `hello-world`'s public API
// without pulling that dev-dependency.
