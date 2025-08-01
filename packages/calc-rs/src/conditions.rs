use std::{
    hash::{DefaultHasher, Hasher},
    vec,
};

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    to_json_binary, Addr, Coin, Decimal, Deps, Env, StdError, StdResult, Timestamp,
};
use rujira_rs::{
    fin::{OrderResponse, Price, QueryMsg, Side},
    query::Pool,
    Layer1Asset,
};

use crate::{
    actions::{limit_order::Direction, swaps::swap::Swap},
    core::Threshold,
    manager::{ManagerQueryMsg, StrategyHandle, StrategyStatus},
};

#[cw_serde]
pub struct CompositeCondition {
    pub conditions: Vec<Condition>,
    pub threshold: Threshold,
}

#[cw_serde]
pub enum Condition {
    TimestampElapsed(Timestamp),
    BlocksCompleted(u64),
    CanSwap(Swap),
    LimitOrderFilled {
        owner: Addr,
        pair_address: Addr,
        side: Side,
        price: Decimal,
    },
    BalanceAvailable {
        address: Addr,
        amount: Coin,
    },
    StrategyBalanceAvailable {
        amount: Coin,
    },
    StrategyStatus {
        manager_contract: Addr,
        contract_address: Addr,
        status: StrategyStatus,
    },
    OraclePrice {
        asset: String,
        direction: Direction,
        rate: Decimal,
    },
    Not(Box<Condition>),
    Composite(CompositeCondition),
}

impl Condition {
    pub fn size(&self) -> usize {
        match self {
            Condition::TimestampElapsed(_) => 1,
            Condition::BlocksCompleted(_) => 1,
            Condition::CanSwap { .. } => 2,
            Condition::LimitOrderFilled { .. } => 2,
            Condition::BalanceAvailable { .. } => 1,
            Condition::StrategyBalanceAvailable { .. } => 1,
            Condition::StrategyStatus { .. } => 2,
            Condition::OraclePrice { .. } => 2,
            Condition::Not(condition) => condition.size(),
            Condition::Composite(CompositeCondition {
                conditions,
                threshold: _,
            }) => conditions.iter().map(|c| c.size()).sum::<usize>() + 1,
        }
    }

    pub fn id(&self, owner: Addr) -> StdResult<u64> {
        let salt_data = to_json_binary(&(owner, self.clone()))?;
        let mut hash = DefaultHasher::new();
        hash.write(salt_data.as_slice());
        Ok(hash.finish())
    }

    pub fn is_satisfied(&self, deps: Deps, env: &Env) -> StdResult<bool> {
        Ok(match self {
            Condition::TimestampElapsed(timestamp) => env.block.time > *timestamp,
            Condition::BlocksCompleted(height) => env.block.height > *height,
            Condition::LimitOrderFilled {
                owner,
                pair_address,
                side,
                price,
            } => {
                let order = deps.querier.query_wasm_smart::<OrderResponse>(
                    pair_address,
                    &QueryMsg::Order((
                        owner.to_string(),
                        side.clone(),
                        Price::Fixed(price.clone()),
                    )),
                )?;

                order.remaining.is_zero()
            }
            Condition::CanSwap(swap) => swap.best_route(deps, env)?.is_some(),
            Condition::BalanceAvailable { address, amount } => {
                let balance = deps.querier.query_balance(address, amount.denom.clone())?;
                balance.amount >= amount.amount
            }
            Condition::StrategyBalanceAvailable { amount } => {
                let balance = deps
                    .querier
                    .query_balance(&env.contract.address, amount.denom.clone())?;
                balance.amount >= amount.amount
            }
            Condition::StrategyStatus {
                manager_contract,
                contract_address,
                status,
            } => {
                let strategy = deps.querier.query_wasm_smart::<StrategyHandle>(
                    manager_contract,
                    &ManagerQueryMsg::Strategy {
                        address: contract_address.clone(),
                    },
                )?;
                strategy.status == *status
            }
            Condition::OraclePrice {
                asset,
                direction,
                rate,
            } => {
                let layer_1_asset = Layer1Asset::from_native(asset.clone()).map_err(|e| {
                    StdError::generic_err(format!(
                        "Denom ({asset}) not a secured asset, error: {e}"
                    ))
                })?;

                let oracle_price = Pool::load(deps.querier, &layer_1_asset)
                    .map_err(|e| {
                        StdError::generic_err(format!(
                            "Failed to load oracle price for {asset}, error: {e}"
                        ))
                    })?
                    .asset_tor_price;

                match direction {
                    Direction::Above => oracle_price > *rate,
                    Direction::Below => oracle_price < *rate,
                }
            }
            Condition::Not(condition) => !condition.is_satisfied(deps, env)?,
            Condition::Composite(CompositeCondition {
                conditions,
                threshold,
            }) => match threshold {
                Threshold::All => conditions
                    .iter()
                    .map(|c| c.is_satisfied(deps, env))
                    .collect::<StdResult<Vec<bool>>>()?
                    .into_iter()
                    .all(|b| b),
                Threshold::Any => conditions
                    .iter()
                    .map(|c| c.is_satisfied(deps, env))
                    .collect::<StdResult<Vec<bool>>>()?
                    .into_iter()
                    .any(|b| b),
            },
        })
    }
}

#[cfg(test)]
mod conditions_tests {
    use super::*;
    use std::str::FromStr;

    use cosmwasm_std::{
        testing::{mock_dependencies, mock_env},
        to_json_binary, Addr, Coin, ContractResult, Decimal, SystemResult, Timestamp, Uint128,
    };
    use rujira_rs::fin::{OrderResponse, Price, Side, SimulationResponse};

    use crate::{
        actions::{
            swaps::fin::FinRoute,
            swaps::swap::{SwapAmountAdjustment, SwapRoute},
        },
        manager::{StrategyHandle, StrategyStatus},
    };

    #[test]
    fn timestamp_elapsed_check() {
        let deps = mock_dependencies();
        let env = mock_env();

        assert!(Condition::TimestampElapsed(env.block.time.minus_seconds(1))
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());

        assert!(!Condition::TimestampElapsed(env.block.time)
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());

        assert!(!Condition::TimestampElapsed(env.block.time.plus_seconds(1))
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());
    }

    #[test]
    fn blocks_completed_check() {
        let deps = mock_dependencies();
        let env = mock_env();

        assert!(Condition::BlocksCompleted(0)
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());
        assert!(Condition::BlocksCompleted(env.block.height - 1)
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());
        assert!(!Condition::BlocksCompleted(env.block.height)
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());
        assert!(!Condition::BlocksCompleted(env.block.height + 1)
            .is_satisfied(deps.as_ref(), &env)
            .unwrap());
    }

    #[test]
    fn balance_available_check() {
        let mut deps = mock_dependencies();
        let env = mock_env();

        assert!(Condition::BalanceAvailable {
            address: env.contract.address.clone(),
            amount: Coin::new(0u128, "rune"),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(!Condition::BalanceAvailable {
            address: env.contract.address.clone(),
            amount: Coin::new(1u128, "rune"),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        deps.querier.bank.update_balance(
            env.contract.address.clone(),
            vec![Coin::new(100u128, "rune")],
        );

        assert!(Condition::BalanceAvailable {
            address: env.contract.address.clone(),
            amount: Coin::new(99u128, "rune"),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(Condition::BalanceAvailable {
            address: env.contract.address.clone(),
            amount: Coin::new(100u128, "rune"),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(!Condition::BalanceAvailable {
            address: env.contract.address.clone(),
            amount: Coin::new(101u128, "rune"),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());
    }

    #[test]
    fn can_swap_check() {
        let mut deps = mock_dependencies();
        let env = mock_env();

        deps.querier.update_wasm(|_| {
            SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&SimulationResponse {
                    returned: Uint128::new(100),
                    fee: Uint128::new(1),
                })
                .unwrap(),
            ))
        });

        assert!(!Condition::CanSwap(Swap {
            swap_amount: Coin::new(100u128, "rune"),
            minimum_receive_amount: Coin::new(101u128, "rune"),
            routes: vec![SwapRoute::Fin(FinRoute {
                pair_address: Addr::unchecked("fin_pair")
            })],
            maximum_slippage_bps: 100,
            adjustment: SwapAmountAdjustment::Fixed
        })
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(!Condition::CanSwap(Swap {
            swap_amount: Coin::new(100u128, "rune"),
            minimum_receive_amount: Coin::new(100u128, "rune"),
            routes: vec![SwapRoute::Fin(FinRoute {
                pair_address: Addr::unchecked("fin_pair")
            })],
            maximum_slippage_bps: 100,
            adjustment: SwapAmountAdjustment::Fixed
        })
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(!Condition::CanSwap(Swap {
            swap_amount: Coin::new(100u128, "rune"),
            minimum_receive_amount: Coin::new(99u128, "rune"),
            routes: vec![SwapRoute::Fin(FinRoute {
                pair_address: Addr::unchecked("fin_pair")
            })],
            maximum_slippage_bps: 100,
            adjustment: SwapAmountAdjustment::Fixed
        })
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());
    }

    #[test]
    fn limit_order_filled_check() {
        let mut deps = mock_dependencies();
        let env = mock_env();

        deps.querier.update_wasm(move |_| {
            SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&OrderResponse {
                    remaining: Uint128::new(100),
                    filled: Uint128::new(100),
                    owner: "owner".to_string(),
                    side: Side::Base,
                    price: Price::Fixed(Decimal::from_str("1.0").unwrap()),
                    rate: Decimal::from_str("1.0").unwrap(),
                    updated_at: Timestamp::from_seconds(env.block.time.seconds()),
                    offer: Uint128::new(21029),
                })
                .unwrap(),
            ))
        });

        assert!(!Condition::LimitOrderFilled {
            owner: Addr::unchecked("owner"),
            pair_address: Addr::unchecked("pair"),
            side: Side::Base,
            price: Decimal::from_str("1.0").unwrap(),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        deps.querier.update_wasm(move |_| {
            SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&OrderResponse {
                    remaining: Uint128::new(0),
                    filled: Uint128::new(100),
                    owner: "owner".to_string(),
                    side: Side::Base,
                    price: Price::Fixed(Decimal::from_str("1.0").unwrap()),
                    rate: Decimal::from_str("1.0").unwrap(),
                    updated_at: Timestamp::from_seconds(env.block.time.seconds()),
                    offer: Uint128::new(21029),
                })
                .unwrap(),
            ))
        });

        assert!(Condition::LimitOrderFilled {
            owner: Addr::unchecked("owner"),
            pair_address: Addr::unchecked("pair"),
            side: Side::Base,
            price: Decimal::from_str("1.0").unwrap(),
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());
    }

    #[test]
    fn strategy_status_check() {
        let mut deps = mock_dependencies();
        let env = mock_env();

        deps.querier.update_wasm(move |_| {
            SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&StrategyHandle {
                    id: 1,
                    contract_address: Addr::unchecked("strategy"),
                    status: StrategyStatus::Active,
                    owner: Addr::unchecked("owner"),
                    created_at: 0,
                    updated_at: 0,
                    label: "label".to_string(),
                    affiliates: vec![],
                })
                .unwrap(),
            ))
        });

        let strategy_address = Addr::unchecked("strategy");

        assert!(Condition::StrategyStatus {
            manager_contract: Addr::unchecked("manager"),
            contract_address: strategy_address.clone(),
            status: StrategyStatus::Active,
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(!Condition::StrategyStatus {
            manager_contract: Addr::unchecked("manager"),
            contract_address: strategy_address.clone(),
            status: StrategyStatus::Paused,
        }
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());
    }

    #[test]
    fn not_satisfied_check() {
        let deps = mock_dependencies();
        let env = mock_env();

        assert!(
            !Condition::Not(Box::new(Condition::BlocksCompleted(env.block.height - 1)))
                .is_satisfied(deps.as_ref(), &env)
                .unwrap()
        );
        assert!(
            Condition::Not(Box::new(Condition::BlocksCompleted(env.block.height)))
                .is_satisfied(deps.as_ref(), &env)
                .unwrap()
        );
    }

    #[test]
    fn composite_condition_check() {
        let deps = mock_dependencies();
        let env = mock_env();

        assert!(!Condition::Composite(CompositeCondition {
            conditions: vec![
                Condition::BlocksCompleted(env.block.height - 1),
                Condition::BlocksCompleted(env.block.height),
            ],
            threshold: Threshold::All,
        })
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());

        assert!(Condition::Composite(CompositeCondition {
            conditions: vec![
                Condition::BlocksCompleted(env.block.height - 1),
                Condition::BlocksCompleted(env.block.height),
            ],
            threshold: Threshold::Any,
        })
        .is_satisfied(deps.as_ref(), &env)
        .unwrap());
    }
}
