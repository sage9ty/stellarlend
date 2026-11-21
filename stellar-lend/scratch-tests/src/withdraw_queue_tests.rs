mod tests {
    use hello_world::withdraw_queue::*;
    use hello_world::errors::LendingError;
    use soroban_sdk::{Address, Env, Vec};
    use hello_world::deposit::{DepositDataKey, Position};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::testutils::Ledger as _;
    use crate::harness::Ctx;

    /// Register the throwaway contract and return its address, used as the
    /// storage context for every test.
    fn new_contract(env: &Env) -> Address {
        env.register(Ctx, ())
    }

    /// Set the protocol admin. `hello-world` reads the admin from
    /// `admin::AdminDataKey::Admin` (not `storage::GovernanceDataKey::Admin`).
    fn set_admin(env: &Env, admin: &Address) {
        env.storage()
            .persistent()
            .set(&hello_world::admin::AdminDataKey::Admin, admin);
    }

    /// Stand up a user with enough collateral and a zero-debt position so the
    /// canonical withdraw path succeeds in tests. With `asset = None` the token
    /// transfer in `withdraw_collateral` is skipped, so no token contract is
    /// needed.
    fn give_collateral(env: &Env, user: &Address, amount: i128) {
        env.storage().persistent().set(
            &DepositDataKey::CollateralBalance(user.clone()),
            &amount,
        );
        env.storage().persistent().set(
            &DepositDataKey::Position(user.clone()),
            &Position {
                collateral: amount,
                debt: 0,
                borrow_interest: 0,
                last_accrual_time: env.ledger().timestamp(),
            },
        );
    }

    fn seed_analytics(env: &Env, total_deposits: i128, total_borrows: i128) {
        env.storage().persistent().set(
            &DepositDataKey::ProtocolAnalytics,
            &hello_world::deposit::ProtocolAnalytics {
                total_deposits,
                total_borrows,
                total_value_locked: total_deposits,
            },
        );
    }

    #[test]
    fn test_is_pinned_threshold_logic() {
        // Pinned exactly at threshold (≥).
        assert!(is_pinned(9_500, 9_500));
        assert!(is_pinned(9_999, 9_500));
        // Below threshold is not pinned.
        assert!(!is_pinned(9_499, 9_500));
        assert!(!is_pinned(0, 9_500));
        // Threshold 0 ⇒ always pinned (used to force the queue path in tests).
        assert!(is_pinned(0, 0));
        // Threshold 10_000 (100%) ⇒ pinned only at exactly 100% utilization;
        // tests run at 0% util so this forces the instant path.
        assert!(!is_pinned(9_999, 10_000));
        assert!(is_pinned(10_000, 10_000));
    }

    #[test]
    fn test_config_defaults_and_validation() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            let cfg = get_config(&env, &pool);
            assert_eq!(cfg.max_queue_size, DEFAULT_MAX_QUEUE_SIZE);
            assert_eq!(cfg.utilization_pin_threshold_bps, DEFAULT_PIN_THRESHOLD_BPS);
            assert_eq!(cfg.max_retries, DEFAULT_MAX_RETRIES);
        });

        // Invalid configs are rejected (each in its own auth frame).
        assert_eq!(
            env.as_contract(&contract, || {
                set_config(
                    &env,
                    &admin,
                    &pool,
                    WithdrawQueueConfig {
                        max_queue_size: 0,
                        ..Default::default()
                    },
                )
            }),
            Err(LendingError::InvalidParameter)
        );
        assert_eq!(
            env.as_contract(&contract, || {
                set_config(
                    &env,
                    &admin,
                    &pool,
                    WithdrawQueueConfig {
                        utilization_pin_threshold_bps: 10_001,
                        ..Default::default()
                    },
                )
            }),
            Err(LendingError::InvalidParameter)
        );
    }

    #[test]
    fn test_set_config_requires_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let other = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
        });

        // Non-admin cannot configure the queue.
        assert_eq!(
            env.as_contract(&contract, || {
                set_config(&env, &other, &pool, WithdrawQueueConfig::default())
            }),
            Err(LendingError::Unauthorized)
        );
        // Admin can.
        assert_eq!(
            env.as_contract(&contract, || {
                set_config(&env, &admin, &pool, WithdrawQueueConfig::default())
            }),
            Ok(())
        );
        env.as_contract(&contract, || {
            let stored = get_config(&env, &pool);
            assert_eq!(stored, WithdrawQueueConfig::default());
        });
    }

    #[test]
    fn test_request_queues_when_pinned() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        let outcome = env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 500).unwrap()
        });
        assert!(outcome.queued);
        assert_eq!(outcome.entry_id, 0);
        assert_eq!(outcome.position, 1);

        env.as_contract(&contract, || {
            let entry = get_request(&env, 0).unwrap();
            assert_eq!(entry.amount, 500);
            assert_eq!(entry.status, WithdrawQueueStatus::Pending);
            assert_eq!(entry.user, user);

            let stats = get_stats(&env, &pool);
            assert_eq!(stats.total_requests, 1);
            assert_eq!(stats.pending, 1);
            assert_eq!(stats.total_amount_queued, 500);
        });
    }

    #[test]
    fn test_request_fulfils_instantly_when_not_pinned() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: NEVER_PINNED_BPS,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        let outcome = env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 500).unwrap()
        });
        assert!(!outcome.queued);
        assert_eq!(outcome.fulfilled_amount, 500);

        env.as_contract(&contract, || {
            // No queue entry created for the instant path.
            assert!(get_request(&env, 0).is_none());
            let stats = get_stats(&env, &pool);
            assert_eq!(stats.total_requests, 0);

            // Collateral was debited by the canonical withdraw path.
            let bal: i128 = env
                .storage()
                .persistent()
                .get(&DepositDataKey::CollateralBalance(user.clone()))
                .unwrap_or(0);
            assert_eq!(bal, 999_500);
        });
    }

    #[test]
    fn test_request_rejects_invalid_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        assert_eq!(
            env.as_contract(&contract, || {
                request_withdrawal(&env, pool.clone(), user.clone(), 0)
            }),
            Err(LendingError::InvalidAmount)
        );
        assert_eq!(
            env.as_contract(&contract, || {
                request_withdrawal(&env, pool.clone(), user.clone(), -1)
            }),
            Err(LendingError::InvalidAmount)
        );
    }

    #[test]
    fn test_fifo_ordering_and_position() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);

        // Setup: admin, pin config, and three collateralised users (returned
        // from the frame so the request frame can use the same addresses).
        let users = env.as_contract(&contract, || {
            set_admin(&env, &admin);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut users = Vec::new(&env);
            for i in 0..3 {
                let u = Address::generate(&env);
                give_collateral(&env, &u, 1_000_000 + i as i128);
                users.push_back(u);
            }
            users
        });

        // Three different users, each authenticating once → safe in one frame.
        let outcomes = env.as_contract(&contract, || {
            let o0 = request_withdrawal(&env, pool.clone(), users.get(0).unwrap().clone(), 100).unwrap();
            let o1 = request_withdrawal(&env, pool.clone(), users.get(1).unwrap().clone(), 200).unwrap();
            let o2 = request_withdrawal(&env, pool.clone(), users.get(2).unwrap().clone(), 300).unwrap();
            (o0, o1, o2)
        });
        assert!(outcomes.0.queued && outcomes.0.entry_id == 0 && outcomes.0.position == 1);
        assert!(outcomes.1.queued && outcomes.1.entry_id == 1 && outcomes.1.position == 2);
        assert!(outcomes.2.queued && outcomes.2.entry_id == 2 && outcomes.2.position == 3);

        env.as_contract(&contract, || {
            let pending = get_pending_requests(&env, &pool);
            assert_eq!(pending.len(), 3);
            assert_eq!(pending.get(0).unwrap().amount, 100);
            assert_eq!(pending.get(1).unwrap().amount, 200);
            assert_eq!(pending.get(2).unwrap().amount, 300);
        });
    }

    #[test]
    fn test_queue_size_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    max_queue_size: 1,
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        let first = env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
        });
        assert!(first.queued);
        // Second request exceeds the cap.
        assert_eq!(
            env.as_contract(&contract, || {
                request_withdrawal(&env, pool.clone(), user.clone(), 100)
            }),
            Err(LendingError::LimitExceeded)
        );
    }

    #[test]
    fn test_cancel_request() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let stranger = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
        });

        // Stranger cannot cancel.
        assert_eq!(
            env.as_contract(&contract, || cancel_request(&env, 0, stranger)),
            Err(LendingError::Unauthorized)
        );
        // User cancels their own request.
        assert_eq!(
            env.as_contract(&contract, || cancel_request(&env, 0, user.clone())),
            Ok(())
        );
        // Cannot cancel twice.
        assert_eq!(
            env.as_contract(&contract, || cancel_request(&env, 0, user.clone())),
            Err(LendingError::InvalidState)
        );

        env.as_contract(&contract, || {
            let entry = get_request(&env, 0).unwrap();
            assert_eq!(entry.status, WithdrawQueueStatus::Cancelled);
            assert!(get_pending_requests(&env, &pool).is_empty());
        });
    }

    #[test]
    fn test_cancel_missing_request() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let user = Address::generate(&env);
        assert_eq!(
            env.as_contract(&contract, || cancel_request(&env, 999, user)),
            Err(LendingError::DataNotFound)
        );
    }

    #[test]
    fn test_process_withdrawals_fulfils_when_liquidity_available() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let keeper = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
            seed_analytics(&env, 10_000, 0);
        });

        // Two queued requests (same user → separate auth frames).
        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 300).unwrap()
        });
        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 200).unwrap()
        });

        let res = env.as_contract(&contract, || {
            process_withdrawals(&env, pool.clone(), keeper.clone(), 10).unwrap()
        });
        assert_eq!(res.fulfilled, 2);
        assert_eq!(res.failed, 0);
        assert_eq!(res.total_fulfilled_amount, 500);

        env.as_contract(&contract, || {
            let stats = get_stats(&env, &pool);
            assert_eq!(stats.fulfilled, 2);
            assert_eq!(stats.pending, 0);
            assert_eq!(stats.total_amount_fulfilled, 500);
        });
    }

    #[test]
    fn test_process_withdrawals_skips_over_large_request() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let keeper = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
            // Only 200 available: the 10_000 request is skipped, the 100 fits.
            seed_analytics(&env, 200, 0);
        });

        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 10_000).unwrap()
        });
        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
        });

        let res = env.as_contract(&contract, || {
            process_withdrawals(&env, pool.clone(), keeper.clone(), 10).unwrap()
        });
        assert_eq!(res.fulfilled, 1);
        assert_eq!(res.total_fulfilled_amount, 100);

        env.as_contract(&contract, || {
            let pending = get_pending_requests(&env, &pool);
            assert_eq!(pending.len(), 1);
            assert_eq!(pending.get(0).unwrap().amount, 10_000);
        });
    }

    #[test]
    fn test_process_withdrawals_retries_then_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let keeper = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            // No collateral ⇒ withdraw_collateral always fails.
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    max_retries: 2,
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
            // Provide liquidity so the request is attempted (not skipped).
            seed_analytics(&env, 10_000, 0);
        });

        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
        });

        // Attempt 1 → retry (retries=1, still pending).
        let r1 = env.as_contract(&contract, || {
            process_withdrawals(&env, pool.clone(), keeper.clone(), 1).unwrap()
        });
        assert_eq!(r1.fulfilled, 0);
        assert_eq!(r1.failed, 0);

        // Attempt 2 → retries reach max_retries ⇒ failed.
        let r2 = env.as_contract(&contract, || {
            process_withdrawals(&env, pool.clone(), keeper.clone(), 1).unwrap()
        });
        assert_eq!(r2.fulfilled, 0);
        assert_eq!(r2.failed, 1);

        env.as_contract(&contract, || {
            let entry = get_request(&env, 0).unwrap();
            assert_eq!(entry.retries, 2);
            assert_eq!(entry.status, WithdrawQueueStatus::Failed);
        });
    }

    #[test]
    fn test_cleanup_expired() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    entry_expiration_seconds: 100,
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
        });

        env.as_contract(&contract, || {
            // Advance time past expiry.
            env.ledger().set_timestamp(env.ledger().timestamp() + 200);
            let cleaned = cleanup_expired(&env, &pool);
            assert_eq!(cleaned, 1);
            assert_eq!(
                get_request(&env, 0).unwrap().status,
                WithdrawQueueStatus::Expired
            );

            let stats = get_stats(&env, &pool);
            assert_eq!(stats.expired, 1);
            assert_eq!(stats.pending, 0);
        });
    }

    #[test]
    fn test_backlog_alert_raised() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 10_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    max_queue_size: 4,
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        // half = 4/2 = 2; > half needs ≥3, and three_quarters = 3 ⇒ critical.
        for _ in 0..3 {
            env.as_contract(&contract, || {
                request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
            });
        }

        env.as_contract(&contract, || {
            let alerts = get_alerts(&env, &pool, 10);
            assert!(!alerts.is_empty());
            let last = alerts.get(alerts.len() - 1).unwrap();
            assert_eq!(last.severity, WithdrawAlertSeverity::Critical);
            assert_eq!(last.queue_size, 3);

            let stats = get_stats(&env, &pool);
            assert!(stats.alerts_raised >= 1);
        });
    }

    #[test]
    fn test_stats_and_views_consistency() {
        let env = Env::default();
        env.mock_all_auths();
        let contract = new_contract(&env);
        let pool: Option<Address> = None;
        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        env.as_contract(&contract, || {
            set_admin(&env, &admin);
            give_collateral(&env, &user, 1_000_000);
            set_config(
                &env,
                &admin,
                &pool,
                WithdrawQueueConfig {
                    utilization_pin_threshold_bps: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        });

        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 100).unwrap()
        });
        env.as_contract(&contract, || {
            request_withdrawal(&env, pool.clone(), user.clone(), 200).unwrap()
        });
        env.as_contract(&contract, || {
            cancel_request(&env, 0, user.clone()).unwrap()
        });

        env.as_contract(&contract, || {
            let stats = get_stats(&env, &pool);
            assert_eq!(stats.total_requests, 2);
            assert_eq!(stats.pending, 1);
            assert_eq!(stats.cancelled, 1);
            assert_eq!(stats.total_amount_queued, 300);
        });
    }
}
