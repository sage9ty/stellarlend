mod tests {
    use hello_world::strategy::*;
    use hello_world::errors::LendingError;
    use soroban_sdk::{Address, Env, Symbol};
    use soroban_sdk::testutils::Address as _;
    use crate::harness::Ctx;

    /// Register the throwaway contract and return its address, used as the
    /// storage context for every test.
    fn new_contract(env: &Env) -> Address {
        env.register(Ctx, ())
    }

    /// Set the protocol admin and return it. `hello-world` reads the admin from
    /// `admin::AdminDataKey::Admin` (not `storage::GovernanceDataKey::Admin`).
    /// Runs in its own contract frame so the caller frames stay auth-clean.
    fn make_admin(env: &Env, contract: &Address) -> Address {
        env.as_contract(contract, || {
            let admin = Address::generate(env);
            env.storage()
                .persistent()
                .set(&hello_world::admin::AdminDataKey::Admin, &admin);
            admin
        })
    }

    fn register(env: &Env, contract: &Address, admin: &Address, name: &str, cap: i128) -> u64 {
        env.as_contract(contract, || {
            register_strategy(
                env,
                admin,
                Address::generate(env),
                Symbol::new(env, name),
                cap,
            )
            .unwrap()
        })
    }

    #[test]
    fn test_register_and_list() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);

        let id0 = register(&env, &contract, &admin, "vault_a", 1_000);
        let id1 = register(&env, &contract, &admin, "vault_b", 2_000);
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);

        env.as_contract(&contract, || {
            let strategies = list_strategies(&env);
            assert_eq!(strategies.len(), 2);
            assert_eq!(strategies.get(0).unwrap().name, Symbol::new(&env, "vault_a"));
            assert_eq!(strategies.get(1).unwrap().max_allocation, 2_000);

            let stats = get_registry_stats(&env);
            assert_eq!(stats.total_strategies, 2);
            assert_eq!(stats.active, 2);
            assert_eq!(stats.total_current, 0);
        });
    }

    #[test]
    fn test_register_requires_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let other = Address::generate(&env);

        assert_eq!(
            env.as_contract(&contract, || {
                register_strategy(
                    &env,
                    &other,
                    Address::generate(&env),
                    Symbol::new(&env, "x"),
                    1_000,
                )
            }),
            Err(LendingError::Unauthorized)
        );
        // Admin succeeds.
        assert!(env.as_contract(&contract, || {
            register_strategy(
                &env,
                &admin,
                Address::generate(&env),
                Symbol::new(&env, "x"),
                1_000,
            )
            .is_ok()
        }));
    }

    #[test]
    fn test_register_rejects_bad_cap_and_capacity() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);

        // Negative cap rejected.
        assert_eq!(
            env.as_contract(&contract, || {
                register_strategy(
                    &env,
                    &admin,
                    Address::generate(&env),
                    Symbol::new(&env, "x"),
                    -1,
                )
            }),
            Err(LendingError::InvalidAmount)
        );

        // Capacity limit enforced.
        env.as_contract(&contract, || {
            set_registry_config(
                &env,
                &admin,
                RegistryConfig {
                    max_strategies: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        });
        register(&env, &contract, &admin, "a", 1_000);
        assert_eq!(
            env.as_contract(&contract, || {
                register_strategy(
                    &env,
                    &admin,
                    Address::generate(&env),
                    Symbol::new(&env, "b"),
                    1_000,
                )
            }),
            Err(LendingError::LimitExceeded)
        );
    }

    #[test]
    fn test_set_target_and_route() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);

        let id = register(&env, &contract, &admin, "vault", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 400).unwrap();
        });

        // current=0, target=400 ⇒ deploy 400.
        let plan = env.as_contract(&contract, || route(&env, &admin).unwrap());
        assert_eq!(plan.len(), 1);
        let instr = plan.get(0).unwrap();
        assert_eq!(instr.strategy_id, id);
        assert_eq!(instr.deploy, 400);
        assert_eq!(instr.withdraw, 0);

        env.as_contract(&contract, || {
            let stats = get_registry_stats(&env);
            assert_eq!(stats.total_target, 400);
        });
    }

    #[test]
    fn test_target_bounds() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);

        assert_eq!(
            env.as_contract(&contract, || set_target_allocation(&env, &admin, id, -1)),
            Err(LendingError::InvalidParameter)
        );
        assert_eq!(
            env.as_contract(&contract, || set_target_allocation(&env, &admin, id, 1_001)),
            Err(LendingError::InvalidParameter)
        );
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 1_000).unwrap();
        });
    }

    #[test]
    fn test_deploy_and_withdraw() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 500).unwrap();
        });

        // Deploy up to target.
        assert_eq!(
            env.as_contract(&contract, || deploy(&env, &admin, id, 200).unwrap()),
            200
        );
        assert_eq!(
            env.as_contract(&contract, || deploy(&env, &admin, id, 300).unwrap()),
            500
        );

        // Cannot deploy past target.
        assert_eq!(
            env.as_contract(&contract, || deploy(&env, &admin, id, 1)),
            Err(LendingError::InvalidState)
        );

        // Withdraw.
        assert_eq!(
            env.as_contract(&contract, || withdraw(&env, &admin, id, 100).unwrap()),
            400
        );
        let cur = env.as_contract(&contract, || {
            get_strategy(&env, id).unwrap().current_allocation
        });
        assert_eq!(cur, 400);

        // Cannot withdraw more than current.
        assert_eq!(
            env.as_contract(&contract, || withdraw(&env, &admin, id, 10_000)),
            Err(LendingError::InsufficientBalance)
        );
    }

    #[test]
    fn test_deploy_enforces_cap() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 1_000).unwrap();
        });
        env.as_contract(&contract, || {
            deploy(&env, &admin, id, 1_000).unwrap();
        });
        // current == cap == target == 1_000; deploying any positive amount
        // breaches the hard cap first ⇒ LimitExceeded.
        assert_eq!(
            env.as_contract(&contract, || deploy(&env, &admin, id, 1)),
            Err(LendingError::LimitExceeded)
        );
    }

    #[test]
    fn test_deploy_rejects_inactive_and_deprecated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 500).unwrap();
        });

        // Inactive blocks new deploys.
        env.as_contract(&contract, || {
            update_strategy(&env, &admin, id, None, Some(StrategyStatus::Inactive)).unwrap();
        });
        assert_eq!(
            env.as_contract(&contract, || deploy(&env, &admin, id, 100)),
            Err(LendingError::InvalidState)
        );

        // Withdrawals still work while inactive (no allocation ⇒ insufficient).
        assert_eq!(
            env.as_contract(&contract, || withdraw(&env, &admin, id, 1)),
            Err(LendingError::InsufficientBalance)
        );

        // Reactivate, deploy, then deprecate.
        env.as_contract(&contract, || {
            update_strategy(&env, &admin, id, None, Some(StrategyStatus::Active)).unwrap();
        });
        env.as_contract(&contract, || {
            deploy(&env, &admin, id, 500).unwrap();
        });
        env.as_contract(&contract, || {
            update_strategy(&env, &admin, id, None, Some(StrategyStatus::Deprecated)).unwrap();
        });
        assert_eq!(
            env.as_contract(&contract, || deploy(&env, &admin, id, 1)),
            Err(LendingError::InvalidState)
        );
        // Deprecated cannot be reactivated.
        assert_eq!(
            env.as_contract(&contract, || {
                update_strategy(&env, &admin, id, None, Some(StrategyStatus::Active))
            }),
            Err(LendingError::InvalidState)
        );
        // But withdrawals from deprecated still work.
        assert_eq!(
            env.as_contract(&contract, || withdraw(&env, &admin, id, 200).unwrap()),
            300
        );
    }

    #[test]
    fn test_update_cap_validated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 500).unwrap();
        });
        env.as_contract(&contract, || {
            deploy(&env, &admin, id, 500).unwrap();
        });

        // Lowering cap below current allocation rejected.
        assert_eq!(
            env.as_contract(&contract, || update_strategy(&env, &admin, id, Some(400), None)),
            Err(LendingError::InvalidParameter)
        );
        // Lowering cap below target rejected.
        assert_eq!(
            env.as_contract(&contract, || update_strategy(&env, &admin, id, Some(499), None)),
            Err(LendingError::InvalidParameter)
        );
        // Valid raise.
        env.as_contract(&contract, || {
            update_strategy(&env, &admin, id, Some(2_000), None).unwrap();
        });
        let cap = env.as_contract(&contract, || get_strategy(&env, id).unwrap().max_allocation);
        assert_eq!(cap, 2_000);
    }

    #[test]
    fn test_remove_strategy() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);

        // Set a target ⇒ cannot remove until cleared.
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 100).unwrap();
        });
        assert_eq!(
            env.as_contract(&contract, || remove_strategy(&env, &admin, id)),
            Err(LendingError::InvalidState)
        );

        // Clear target and remove.
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 0).unwrap();
        });
        env.as_contract(&contract, || {
            remove_strategy(&env, &admin, id).unwrap();
        });
        env.as_contract(&contract, || {
            assert!(get_strategy(&env, id).is_none());
            assert_eq!(list_strategies(&env).len(), 0);
        });

        // Removing again ⇒ not found.
        assert_eq!(
            env.as_contract(&contract, || remove_strategy(&env, &admin, id)),
            Err(LendingError::DataNotFound)
        );
    }

    #[test]
    fn test_route_skips_inactive_and_zero_delta() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);

        let active = register(&env, &contract, &admin, "a", 1_000);
        let inactive = register(&env, &contract, &admin, "b", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, active, 300).unwrap();
        });
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, inactive, 300).unwrap();
        });
        env.as_contract(&contract, || {
            update_strategy(&env, &admin, inactive, None, Some(StrategyStatus::Inactive)).unwrap();
        });

        let plan = env.as_contract(&contract, || route(&env, &admin).unwrap());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.get(0).unwrap().strategy_id, active);
    }

    #[test]
    fn test_route_emits_withdraw_when_current_exceeds_target() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id = register(&env, &contract, &admin, "vault", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 500).unwrap();
        });
        env.as_contract(&contract, || {
            deploy(&env, &admin, id, 500).unwrap();
        });

        // Lower target below current ⇒ route should withdraw.
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id, 200).unwrap();
        });
        let plan = env.as_contract(&contract, || route(&env, &admin).unwrap());
        assert_eq!(plan.len(), 1);
        let instr = plan.get(0).unwrap();
        assert_eq!(instr.deploy, 0);
        assert_eq!(instr.withdraw, 300);
    }

    #[test]
    fn test_target_allocations_view() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);
        let id0 = register(&env, &contract, &admin, "a", 1_000);
        let id1 = register(&env, &contract, &admin, "b", 1_000);
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id0, 100).unwrap();
        });
        env.as_contract(&contract, || {
            set_target_allocation(&env, &admin, id1, 200).unwrap();
        });

        env.as_contract(&contract, || {
            let targets = get_target_allocations(&env);
            assert_eq!(targets.get(id0), Some(100));
            assert_eq!(targets.get(id1), Some(200));
        });
    }

    #[test]
    fn test_config_validation() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let admin = make_admin(&env, &contract);

        assert_eq!(
            env.as_contract(&contract, || {
                set_registry_config(
                    &env,
                    &admin,
                    RegistryConfig {
                        max_strategies: 0,
                        ..Default::default()
                    },
                )
            }),
            Err(LendingError::InvalidParameter)
        );
        assert_eq!(
            env.as_contract(&contract, || {
                set_registry_config(
                    &env,
                    &admin,
                    RegistryConfig {
                        min_allocation: -1,
                        ..Default::default()
                    },
                )
            }),
            Err(LendingError::InvalidAmount)
        );
        env.as_contract(&contract, || {
            set_registry_config(&env, &admin, RegistryConfig::default()).unwrap();
        });
    }
}
