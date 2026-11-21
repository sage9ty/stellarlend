//! Test-only helpers shared by the feature test modules.
//!
//! [`Ctx`] is a throwaway contract registered only to obtain a contract
//! address that `Env::as_contract` can key storage against. The feature
//! modules read/write `env.storage()`, which on `soroban-sdk` 28 is only
//! reachable inside a contract frame. `Ctx` has no entrypoints.

use soroban_sdk::{contract, contractimpl};

#[contract]
pub struct Ctx;

#[contractimpl]
impl Ctx {}
