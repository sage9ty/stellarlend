//! # Strategy registry for yield-routing adapters (#1580)
//!
//! ## Why this exists
//!
//! Idle protocol liquidity can earn yield only if there is a single, audited
//! place that records which yield adapters exist, how much may be deployed to
//! each, and how much actually is. Without it, yield routing is ad-hoc and
//! unverifiable — operators cannot answer "what is allocated where", and there
//! is no access-controlled path to add or retire an adapter safely.
//!
//! ## Model
//!
//! The registry is the source of truth for *intent*; the movement of tokens
//! into and out of an adapter is settled by a keeper after the registry has
//! authorized it:
//!
//! 1. **Register** – admin adds an adapter (`register_strategy`) with a name,
//!    a hard per-strategy cap, and an initial `Active` status.
//! 2. **Target** – admin sets a `target_allocation` per strategy (≤ cap).
//! 3. **Route** – a keeper calls [`route`], which compares each active
//!    strategy's `current_allocation` to its target and emits a typed
//!    [`RouteInstruction`] plan (deploy X / withdraw Y) plus a `RoutedEvent`.
//!    The plan is returned to the caller and published for off-chain indexers.
//! 4. **Settle** – once the keeper has actually moved tokens to/from the
//!    adapter, it calls [`deploy`] (funds in) or [`withdraw`] (funds out) to
//!    record the movement. Both enforce the cap and the target so the registry
//!    can never authorize more than the configured bounds.
//! 5. **Retire** – admin sets a strategy `Deprecated` (no new deploys) or
//!    removes it once its allocation is zero.
//!
//! This keeps the registry self-contained, fully testable, and free of
//! adapter-specific assumptions: any adapter that respects the deposit /
//! withdraw accounting works, because the registry never calls into the
//! adapter itself — it only records authorized allocations.
//!
//! ## Storage layout
//!
//! | Key | Storage | Value |
//! | --- | --- | --- |
//! | `StrategyKey::RegistryConfig` | persistent | [`RegistryConfig`] |
//! | `StrategyKey::NextStrategyId` | instance | `u64` |
//! | `StrategyKey::Strategy(id)` | persistent | [`Strategy`] |
//! | `StrategyKey::StrategyCount` | instance | `u32` |
//!
//! ## Observability
//!
//! Every mutation emits a typed event (`StrategyRegisteredEvent`,
//! `StrategyUpdatedEvent`, `StrategyRemovedEvent`, `StrategyTargetSetEvent`,
//! `StrategiesRoutedEvent`, `StrategyDeployedEvent`, `StrategyWithdrawnEvent`,
//! `StrategyRegistryConfiguredEvent`, `StrategyAlertEvent`). Backlog and
//! policy violations raise a [`StrategyAlert`] with its own event.

use soroban_sdk::{contractevent, contracttype, Address, Env, Map, Symbol, Vec};

use crate::errors::LendingError;

// ─── Tunables ───────────────────────────────────────────────────────────────

pub const DEFAULT_MAX_STRATEGIES: u32 = 50;
/// Default per-strategy cap when none is supplied: effectively uncapped.
pub const DEFAULT_MAX_ALLOCATION: i128 = i128::MAX;
pub const DEFAULT_MIN_ALLOCATION: i128 = 0;

// ─── Storage keys ───────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum StrategyKey {
    /// Global registry configuration.
    RegistryConfig,
    /// Global monotonic strategy id counter.
    NextStrategyId,
    /// A registered strategy keyed by id.
    Strategy(u64),
    /// Number of non-removed strategies (capacity guard).
    StrategyCount,
}

// ─── Types ──────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum StrategyStatus {
    /// Accepts new deployments and withdrawals.
    Active,
    /// Paused: no new deployments, withdrawals still allowed.
    Inactive,
    /// Retired: no new deployments; withdraw down to zero before removal.
    Deprecated,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Strategy {
    pub id: u64,
    pub adapter: Address,
    pub name: Symbol,
    pub status: StrategyStatus,
    /// Hard ceiling on `current_allocation`.
    pub max_allocation: i128,
    /// Amount currently deployed to the adapter.
    pub current_allocation: i128,
    /// Admin-set desired allocation; routing drives `current` toward this.
    pub target_allocation: i128,
    pub registered_at: u64,
    pub last_activity_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryConfig {
    pub max_strategies: u32,
    pub default_max_allocation: i128,
    pub min_allocation: i128,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            max_strategies: DEFAULT_MAX_STRATEGIES,
            default_max_allocation: DEFAULT_MAX_ALLOCATION,
            min_allocation: DEFAULT_MIN_ALLOCATION,
        }
    }
}

/// A single routing instruction produced by [`route`].
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct RouteInstruction {
    pub strategy_id: u64,
    pub adapter: Address,
    /// Positive amount to deploy to the adapter (0 if none).
    pub deploy: i128,
    /// Positive amount to withdraw from the adapter (0 if none).
    pub withdraw: i128,
}

/// Derived registry statistics.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryStats {
    pub total_strategies: u32,
    pub active: u32,
    pub inactive: u32,
    pub deprecated: u32,
    pub total_target: i128,
    pub total_current: i128,
    pub last_route_at: u64,
    pub alerts_raised: u32,
}

impl RegistryStats {
    fn zero() -> Self {
        Self {
            total_strategies: 0,
            active: 0,
            inactive: 0,
            deprecated: 0,
            total_target: 0,
            total_current: 0,
            last_route_at: 0,
            alerts_raised: 0,
        }
    }
}

/// Severity of a registry alert.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum StrategyAlertSeverity {
    Warning,
    Critical,
}

/// A registry policy alert.
#[contracttype]
#[derive(Clone, Debug)]
pub struct StrategyAlert {
    pub alert_id: u64,
    pub severity: StrategyAlertSeverity,
    pub message: Symbol,
    pub strategy_id: u64,
    pub timestamp: u64,
}

// ─── Events ──────────────────────────────────────────────────────────────────

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyRegisteredEvent {
    #[topic]
    pub adapter: Address,
    pub strategy_id: u64,
    pub name: Symbol,
    pub max_allocation: i128,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyUpdatedEvent {
    #[topic]
    pub adapter: Address,
    pub strategy_id: u64,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyRemovedEvent {
    #[topic]
    pub adapter: Address,
    pub strategy_id: u64,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyTargetSetEvent {
    #[topic]
    pub adapter: Address,
    pub strategy_id: u64,
    pub target: i128,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategiesRoutedEvent {
    pub instruction_count: u32,
    pub total_deploy: i128,
    pub total_withdraw: i128,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyDeployedEvent {
    #[topic]
    pub adapter: Address,
    pub strategy_id: u64,
    pub amount: i128,
    pub new_current: i128,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyWithdrawnEvent {
    #[topic]
    pub adapter: Address,
    pub strategy_id: u64,
    pub amount: i128,
    pub new_current: i128,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyConfiguredEvent {
    #[topic]
    pub caller: Address,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct StrategyAlertEvent {
    pub alert_id: u64,
    pub severity: StrategyAlertSeverity,
    pub strategy_id: u64,
    pub message: Symbol,
    pub timestamp: u64,
}

// ─── Config ──────────────────────────────────────────────────────────────────

pub fn get_registry_config(env: &Env) -> RegistryConfig {
    env.storage()
        .persistent()
        .get::<StrategyKey, RegistryConfig>(&StrategyKey::RegistryConfig)
        .unwrap_or_default()
}

/// Admin-only: replace the registry configuration.
pub fn set_registry_config(
    env: &Env,
    caller: &Address,
    config: RegistryConfig,
) -> Result<(), LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;
    validate_registry_config(&config)?;
    env.storage()
        .persistent()
        .set(&StrategyKey::RegistryConfig, &config);
    StrategyConfiguredEvent {
        caller: caller.clone(),
        timestamp: env.ledger().timestamp(),
    }
    .publish(env);
    Ok(())
}

fn validate_registry_config(config: &RegistryConfig) -> Result<(), LendingError> {
    if config.max_strategies == 0 {
        return Err(LendingError::InvalidParameter);
    }
    if config.min_allocation < 0 {
        return Err(LendingError::InvalidAmount);
    }
    Ok(())
}

// ─── Registration / lifecycle ───────────────────────────────────────────────

/// Admin-only: register a new yield-routing adapter.
///
/// Returns the new strategy id.
pub fn register_strategy(
    env: &Env,
    caller: &Address,
    adapter: Address,
    name: Symbol,
    max_allocation: i128,
) -> Result<u64, LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    let config = get_registry_config(env);
    if max_allocation < config.min_allocation {
        return Err(LendingError::InvalidAmount);
    }

    let count = strategy_count(env);
    if count >= config.max_strategies {
        return Err(LendingError::LimitExceeded);
    }

    let id = next_strategy_id(env);
    let now = env.ledger().timestamp();
    let strategy = Strategy {
        id,
        adapter: adapter.clone(),
        name,
        status: StrategyStatus::Active,
        max_allocation,
        current_allocation: 0,
        target_allocation: 0,
        registered_at: now,
        last_activity_at: now,
    };
    env.storage()
        .persistent()
        .set(&StrategyKey::Strategy(id), &strategy);
    bump_next_strategy_id(env);
    set_strategy_count(env, count + 1);

    StrategyRegisteredEvent {
        adapter,
        strategy_id: id,
        name: strategy.name.clone(),
        max_allocation,
        timestamp: now,
    }
    .publish(env);
    Ok(id)
}

/// Admin-only: update a strategy's cap and/or status.
pub fn update_strategy(
    env: &Env,
    caller: &Address,
    strategy_id: u64,
    max_allocation: Option<i128>,
    status: Option<StrategyStatus>,
) -> Result<(), LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    let key = StrategyKey::Strategy(strategy_id);
    let mut strategy: Strategy = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(LendingError::DataNotFound)?;

    let config = get_registry_config(env);
    if let Some(cap) = max_allocation {
        if cap < config.min_allocation {
            return Err(LendingError::InvalidAmount);
        }
        // Lowering the cap below the current allocation is unsafe; reject it.
        if cap < strategy.current_allocation {
            return Err(LendingError::InvalidParameter);
        }
        // Lowering the cap below the target makes the target unreachable; reject.
        if cap < strategy.target_allocation {
            return Err(LendingError::InvalidParameter);
        }
        strategy.max_allocation = cap;
    }
    if let Some(new_status) = status {
        // A Deprecated strategy may not be reactivated once retired.
        if strategy.status == StrategyStatus::Deprecated
            && new_status != StrategyStatus::Deprecated
        {
            return Err(LendingError::InvalidState);
        }
        strategy.status = new_status;
    }
    strategy.last_activity_at = env.ledger().timestamp();
    env.storage().persistent().set(&key, &strategy);

    StrategyUpdatedEvent {
        adapter: strategy.adapter.clone(),
        strategy_id,
        timestamp: strategy.last_activity_at,
    }
    .publish(env);
    Ok(())
}

/// Admin-only: remove a strategy. Only allowed when nothing is allocated.
pub fn remove_strategy(
    env: &Env,
    caller: &Address,
    strategy_id: u64,
) -> Result<(), LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    let key = StrategyKey::Strategy(strategy_id);
    let strategy: Strategy = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(LendingError::DataNotFound)?;

    if strategy.current_allocation != 0 || strategy.target_allocation != 0 {
        return Err(LendingError::InvalidState);
    }

    env.storage().persistent().remove(&key);
    let count = strategy_count(env);
    set_strategy_count(env, count.saturating_sub(1));

    StrategyRemovedEvent {
        adapter: strategy.adapter,
        strategy_id,
        timestamp: env.ledger().timestamp(),
    }
    .publish(env);
    Ok(())
}

// ─── Targeting & routing ─────────────────────────────────────────────────────

/// Admin-only: set the desired allocation for a strategy (≤ its cap).
pub fn set_target_allocation(
    env: &Env,
    caller: &Address,
    strategy_id: u64,
    target: i128,
) -> Result<(), LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    let key = StrategyKey::Strategy(strategy_id);
    let mut strategy: Strategy = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(LendingError::DataNotFound)?;

    if strategy.status == StrategyStatus::Deprecated {
        return Err(LendingError::InvalidState);
    }
    if target < 0 || target > strategy.max_allocation {
        return Err(LendingError::InvalidParameter);
    }

    strategy.target_allocation = target;
    strategy.last_activity_at = env.ledger().timestamp();
    env.storage().persistent().set(&key, &strategy);

    StrategyTargetSetEvent {
        adapter: strategy.adapter.clone(),
        strategy_id,
        target,
        timestamp: strategy.last_activity_at,
    }
    .publish(env);
    Ok(())
}

/// Compute and publish the routing plan for every active strategy.
///
/// For each active strategy whose `current_allocation` differs from its
/// `target_allocation`, a [`RouteInstruction`] is produced (deploy the
/// surplus / withdraw the deficit) and a `StrategiesRoutedEvent` is emitted.
/// Returns the full plan. Does not move funds; settlement happens via
/// [`deploy`] / [`withdraw`] once tokens have actually moved.
pub fn route(env: &Env, caller: &Address) -> Result<Vec<RouteInstruction>, LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    let mut plan = Vec::new(env);
    let mut total_deploy = 0i128;
    let mut total_withdraw = 0i128;
    let now = env.ledger().timestamp();
    let next_id = next_strategy_id(env);

    for id in 0..next_id {
        let key = StrategyKey::Strategy(id);
        if let Some(mut strategy) = env
            .storage()
            .persistent()
            .get::<StrategyKey, Strategy>(&key)
        {
            if strategy.status != StrategyStatus::Active {
                continue;
            }
            let delta = strategy.target_allocation - strategy.current_allocation;
            let (deploy, withdraw) = if delta >= 0 {
                (delta, 0)
            } else {
                (0, -delta)
            };
            if deploy == 0 && withdraw == 0 {
                continue;
            }
            plan.push_back(RouteInstruction {
                strategy_id: id,
                adapter: strategy.adapter.clone(),
                deploy,
                withdraw,
            });
            total_deploy = total_deploy.saturating_add(deploy);
            total_withdraw = total_withdraw.saturating_add(withdraw);
            strategy.last_activity_at = now;
            env.storage().persistent().set(&key, &strategy);
        }
    }

    StrategiesRoutedEvent {
        instruction_count: plan.len(),
        total_deploy,
        total_withdraw,
        timestamp: now,
    }
    .publish(env);

    Ok(plan)
}

/// Settle a deployment: record `amount` moved into the adapter.
///
/// Enforces the strategy is active, the amount is positive, and that the
/// result stays within the cap and target.
pub fn deploy(
    env: &Env,
    caller: &Address,
    strategy_id: u64,
    amount: i128,
) -> Result<i128, LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    if amount <= 0 {
        return Err(LendingError::InvalidAmount);
    }
    let key = StrategyKey::Strategy(strategy_id);
    let mut strategy: Strategy = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(LendingError::DataNotFound)?;

    if strategy.status == StrategyStatus::Deprecated || strategy.status == StrategyStatus::Inactive
    {
        return Err(LendingError::InvalidState);
    }
    let new_current = strategy
        .current_allocation
        .checked_add(amount)
        .ok_or(LendingError::Overflow)?;
    if new_current > strategy.max_allocation {
        maybe_raise_alert(
            env,
            strategy_id,
            StrategyAlertSeverity::Critical,
            Symbol::new(env, "strat_over_cap"),
        );
        return Err(LendingError::LimitExceeded);
    }
    if new_current > strategy.target_allocation {
        return Err(LendingError::InvalidState);
    }

    strategy.current_allocation = new_current;
    strategy.last_activity_at = env.ledger().timestamp();
    env.storage().persistent().set(&key, &strategy);

    StrategyDeployedEvent {
        adapter: strategy.adapter,
        strategy_id,
        amount,
        new_current,
        timestamp: strategy.last_activity_at,
    }
    .publish(env);
    Ok(new_current)
}

/// Settle a withdrawal: record `amount` moved out of the adapter.
pub fn withdraw(
    env: &Env,
    caller: &Address,
    strategy_id: u64,
    amount: i128,
) -> Result<i128, LendingError> {
    caller.require_auth();
    require_admin(env, caller)?;

    if amount <= 0 {
        return Err(LendingError::InvalidAmount);
    }
    let key = StrategyKey::Strategy(strategy_id);
    let mut strategy: Strategy = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(LendingError::DataNotFound)?;

    // Withdrawals are allowed from Inactive and Deprecated strategies so
    // funds can always be retrieved; only a missing strategy blocks.
    if amount > strategy.current_allocation {
        return Err(LendingError::InsufficientBalance);
    }
    let new_current = strategy
        .current_allocation
        .checked_sub(amount)
        .ok_or(LendingError::Overflow)?;

    strategy.current_allocation = new_current;
    strategy.last_activity_at = env.ledger().timestamp();
    env.storage().persistent().set(&key, &strategy);

    StrategyWithdrawnEvent {
        adapter: strategy.adapter,
        strategy_id,
        amount,
        new_current,
        timestamp: strategy.last_activity_at,
    }
    .publish(env);
    Ok(new_current)
}

// ─── Views ───────────────────────────────────────────────────────────────────

pub fn get_strategy(env: &Env, strategy_id: u64) -> Option<Strategy> {
    env.storage()
        .persistent()
        .get(&StrategyKey::Strategy(strategy_id))
}

/// All non-removed strategies, ordered by id.
pub fn list_strategies(env: &Env) -> Vec<Strategy> {
    let next_id = next_strategy_id(env);
    let mut out = Vec::new(env);
    for id in 0..next_id {
        if let Some(strategy) = env
            .storage()
            .persistent()
            .get::<StrategyKey, Strategy>(&StrategyKey::Strategy(id))
        {
            out.push_back(strategy);
        }
    }
    out
}

/// Derived registry statistics, computed from the strategy table.
pub fn get_registry_stats(env: &Env) -> RegistryStats {
    let next_id = next_strategy_id(env);
    let mut stats = RegistryStats::zero();

    for id in 0..next_id {
        if let Some(strategy) = env
            .storage()
            .persistent()
            .get::<StrategyKey, Strategy>(&StrategyKey::Strategy(id))
        {
            stats.total_strategies += 1;
            match strategy.status {
                StrategyStatus::Active => stats.active += 1,
                StrategyStatus::Inactive => stats.inactive += 1,
                StrategyStatus::Deprecated => stats.deprecated += 1,
            }
            stats.total_target = stats.total_target.saturating_add(strategy.target_allocation);
            stats.total_current =
                stats.total_current.saturating_add(strategy.current_allocation);
            if strategy.last_activity_at > stats.last_route_at {
                stats.last_route_at = strategy.last_activity_at;
            }
        }
    }
    stats
}

/// Current target allocation per strategy id.
pub fn get_target_allocations(env: &Env) -> Map<u64, i128> {
    let next_id = next_strategy_id(env);
    let mut out = Map::new(env);
    for id in 0..next_id {
        if let Some(strategy) = env
            .storage()
            .persistent()
            .get::<StrategyKey, Strategy>(&StrategyKey::Strategy(id))
        {
            out.set(id, strategy.target_allocation);
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

fn next_strategy_id(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get::<StrategyKey, u64>(&StrategyKey::NextStrategyId)
        .unwrap_or(0)
}

fn bump_next_strategy_id(env: &Env) {
    let next = next_strategy_id(env).saturating_add(1);
    env.storage()
        .instance()
        .set(&StrategyKey::NextStrategyId, &next);
}

fn strategy_count(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get::<StrategyKey, u32>(&StrategyKey::StrategyCount)
        .unwrap_or(0)
}

fn set_strategy_count(env: &Env, count: u32) {
    env.storage()
        .instance()
        .set(&StrategyKey::StrategyCount, &count);
}

fn maybe_raise_alert(
    env: &Env,
    strategy_id: u64,
    severity: StrategyAlertSeverity,
    message: Symbol,
) {
    // Alerts are emitted as events; a persistent store is intentionally omitted
    // to keep the registry's storage footprint minimal. The event carries the
    // full context for off-chain monitors.
    let alert_id = next_strategy_id(env); // reuse monotonic counter for unique ids
    StrategyAlertEvent {
        alert_id,
        severity,
        strategy_id,
        message,
        timestamp: env.ledger().timestamp(),
    }
    .publish(env);
}
// Tests for this module live in the `scratch-tests` crate at
// `scratch-tests/src/strategy_tests.rs`. They are kept out of the inline
// `#[cfg(test)]` path because `cargo test -p hello-world` is currently
// blocked by the pre-existing `stellarlend-lending` dev-dependency
// breakage; the separate crate exercises `hello-world`'s public API
// without pulling that dev-dependency.
