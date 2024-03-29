use axelar_wasm_std::{nonempty, FnExt};
use cosmwasm_std::{Addr, DepsMut, Uint128};
use error_stack::Result;
use std::collections::HashMap;

use crate::{
    error::ContractError,
    msg::Params,
    state::{
        Config, Epoch, EpochTally, Event, RewardsStore, StorageState, Store, StoredParams, CONFIG,
    },
};

const DEFAULT_EPOCHS_TO_PROCESS: u64 = 10;
const EPOCH_PAYOUT_DELAY: u64 = 2;

pub struct Contract<S>
where
    S: Store,
{
    pub store: S,
    pub config: Config,
}

impl<'a> Contract<RewardsStore<'a>> {
    pub fn new(deps: DepsMut) -> Contract<RewardsStore> {
        let config = CONFIG.load(deps.storage).expect("couldn't load config");
        Contract {
            store: RewardsStore {
                storage: deps.storage,
            },
            config,
        }
    }
}

#[allow(dead_code)]
impl<S> Contract<S>
where
    S: Store,
{
    /// Returns the current epoch. The current epoch is computed dynamically based on the current
    /// block height and the epoch duration. If the epoch duration is updated, we store the epoch
    /// in which the update occurs as the last checkpoint
    fn current_epoch(&self, cur_block_height: u64) -> Result<Epoch, ContractError> {
        let stored_params = self.store.load_params();
        let epoch_duration: u64 = stored_params.params.epoch_duration.into();
        let last_updated_epoch = stored_params.last_updated;

        if cur_block_height < last_updated_epoch.block_height_started {
            Err(ContractError::BlockHeightInPast.into())
        } else {
            let epochs_elapsed =
                (cur_block_height - last_updated_epoch.block_height_started) / epoch_duration;
            Ok(Epoch {
                epoch_num: last_updated_epoch.epoch_num + epochs_elapsed,
                block_height_started: last_updated_epoch.block_height_started
                    + (epochs_elapsed * epoch_duration), // result is strictly less than cur_block_height, so multiplication is safe
            })
        }
    }

    fn require_governance(&self, sender: Addr) -> Result<(), ContractError> {
        if self.config.governance != sender {
            return Err(ContractError::Unauthorized.into());
        }
        Ok(())
    }

    pub fn record_participation(
        &mut self,
        event_id: nonempty::String,
        worker: Addr,
        target_contract: Addr,
        block_height: u64,
    ) -> Result<(), ContractError> {
        let cur_epoch = self.current_epoch(block_height)?;

        let event =
            self.load_or_store_event(event_id, target_contract.clone(), cur_epoch.epoch_num)?;

        self.store
            .load_epoch_tally(target_contract.clone(), event.epoch_num)?
            .unwrap_or(EpochTally::new(
                target_contract,
                cur_epoch,
                self.store.load_params().params,
            ))
            .record_participation(worker)
            .then(|mut tally| {
                if matches!(event, StorageState::New(_)) {
                    tally.event_count += 1
                }
                self.store.save_epoch_tally(&tally)
            })
    }

    fn load_or_store_event(
        &mut self,
        event_id: nonempty::String,
        target_contract: Addr,
        cur_epoch_num: u64,
    ) -> Result<StorageState<Event>, ContractError> {
        let event = self
            .store
            .load_event(event_id.to_string(), target_contract.clone())?;

        match event {
            None => {
                let event = Event::new(event_id, target_contract, cur_epoch_num);
                self.store.save_event(&event)?;
                Ok(StorageState::New(event))
            }
            Some(event) => Ok(StorageState::Existing(event)),
        }
    }

    pub fn distribute_rewards(
        &mut self,
        target_contract: Addr,
        cur_block_height: u64,
        epoch_process_limit: Option<u64>,
    ) -> Result<HashMap<Addr, Uint128>, ContractError> {
        let epoch_process_limit = epoch_process_limit.unwrap_or(DEFAULT_EPOCHS_TO_PROCESS);
        let cur_epoch = self.current_epoch(cur_block_height)?;

        let from = self
            .store
            .load_rewards_watermark(target_contract.clone())?
            .map_or(0, |last_processed| last_processed + 1);

        let to = std::cmp::min(
            (from + epoch_process_limit).saturating_sub(1), // for process limit =1 "from" and "to" must be equal
            cur_epoch.epoch_num.saturating_sub(EPOCH_PAYOUT_DELAY),
        );

        if to < from || cur_epoch.epoch_num < EPOCH_PAYOUT_DELAY {
            return Err(ContractError::NoRewardsToDistribute.into());
        }

        let rewards = self.process_rewards_for_epochs(target_contract.clone(), from, to)?;
        self.store.save_rewards_watermark(target_contract, to)?;
        Ok(rewards)
    }

    fn process_rewards_for_epochs(
        &mut self,
        target_contract: Addr,
        from: u64,
        to: u64,
    ) -> Result<HashMap<Addr, Uint128>, ContractError> {
        let rewards = self.cumulate_rewards(&target_contract, from, to);
        self.store
            .load_rewards_pool(target_contract.clone())?
            .sub_reward(rewards.values().sum())?
            .then(|pool| self.store.save_rewards_pool(&pool))?;

        Ok(rewards)
    }

    fn cumulate_rewards(
        &mut self,
        target_contract: &Addr,
        from: u64,
        to: u64,
    ) -> HashMap<Addr, Uint128> {
        self.iterate_epoch_tallies(target_contract, from, to)
            .map(|tally| tally.rewards_by_worker())
            .fold(HashMap::new(), merge_rewards)
    }

    fn iterate_epoch_tallies<'a>(
        &'a mut self,
        target_contract: &'a Addr,
        from: u64,
        to: u64,
    ) -> impl Iterator<Item = EpochTally> + 'a {
        (from..=to).filter_map(|epoch_num| {
            self.store
                .load_epoch_tally(target_contract.clone(), epoch_num)
                .unwrap_or_default()
        })
    }

    pub fn update_params(
        &mut self,
        new_params: Params,
        block_height: u64,
        sender: Addr,
    ) -> Result<(), ContractError> {
        self.require_governance(sender)?;
        let cur_epoch = self.current_epoch(block_height)?;
        // If the param update reduces the epoch duration such that the current epoch immediately ends,
        // start a new epoch at this block, incrementing the current epoch number by 1.
        // This prevents us from jumping forward an arbitrary number of epochs, and maintains consistency for past events.
        // (i.e. we are in epoch 0, which started at block 0 and epoch duration is 1000. At epoch 500, the params
        // are updated to shorten the epoch duration to 100 blocks. We set the epoch number to 1, to prevent skipping
        // epochs 1-4, and so all events prior to the start of epoch 1 have an epoch number of 0)
        let should_end =
            cur_epoch.block_height_started + u64::from(new_params.epoch_duration) < block_height;
        let cur_epoch = if should_end {
            Epoch {
                block_height_started: block_height,
                epoch_num: cur_epoch.epoch_num + 1,
            }
        } else {
            cur_epoch
        };
        self.store.save_params(&StoredParams {
            params: new_params,
            last_updated: cur_epoch,
        })?;
        Ok(())
    }

    pub fn add_rewards(
        &mut self,
        contract: Addr,
        amount: nonempty::Uint128,
    ) -> Result<(), ContractError> {
        let mut pool = self.store.load_rewards_pool(contract.clone())?;
        pool.balance += Uint128::from(amount);

        self.store.save_rewards_pool(&pool)?;

        Ok(())
    }
}

/// Merges rewards_2 into rewards_1. For each (address, amount) pair in rewards_2,
/// adds the rewards amount to the existing rewards amount in rewards_1. If the
/// address is not yet in rewards_1, initializes the rewards amount to the amount in
/// rewards_2
/// Performs a number of inserts equal to the length of rewards_2
fn merge_rewards(
    rewards_1: HashMap<Addr, Uint128>,
    rewards_2: HashMap<Addr, Uint128>,
) -> HashMap<Addr, Uint128> {
    rewards_2
        .into_iter()
        .fold(rewards_1, |mut rewards, (addr, amt)| {
            *rewards.entry(addr).or_default() += amt;
            rewards
        })
}

#[cfg(test)]
mod test {
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    };

    use axelar_wasm_std::nonempty;
    use cosmwasm_std::{Addr, Uint128, Uint64};

    use crate::{
        error::ContractError,
        msg::Params,
        state::{self, Config, Epoch, EpochTally, Event, RewardsPool, Store, StoredParams},
    };

    use super::Contract;

    /// Tests that the current epoch is computed correctly when the expected epoch is the same as the stored epoch
    #[test]
    fn current_epoch_same_epoch_is_idempotent() {
        let cur_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;
        let contract = setup(cur_epoch_num, block_height_started, epoch_duration);
        let new_epoch = contract.current_epoch(block_height_started).unwrap();
        assert_eq!(new_epoch.epoch_num, cur_epoch_num);
        assert_eq!(new_epoch.block_height_started, block_height_started);

        let new_epoch = contract.current_epoch(block_height_started + 1).unwrap();
        assert_eq!(new_epoch.epoch_num, cur_epoch_num);
        assert_eq!(new_epoch.block_height_started, block_height_started);

        let new_epoch = contract
            .current_epoch(block_height_started + epoch_duration - 1)
            .unwrap();
        assert_eq!(new_epoch.epoch_num, cur_epoch_num);
        assert_eq!(new_epoch.block_height_started, block_height_started);
    }

    /// When current epoch is called with a block number that is before the current epoch's start date,
    /// it should return an error
    #[test]
    fn current_epoch_call_with_block_in_the_past() {
        let cur_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;
        let contract = setup(cur_epoch_num, block_height_started, epoch_duration);
        assert!(contract.current_epoch(block_height_started - 1).is_err());
        assert!(contract
            .current_epoch(block_height_started - epoch_duration)
            .is_err());
    }

    /// Tests that the current epoch is computed correctly when the expected epoch is different than the stored epoch
    #[test]
    fn current_epoch_different_epoch() {
        let cur_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;
        let contract = setup(cur_epoch_num, block_height_started, epoch_duration);

        // elements are (height, expected epoch number, expected epoch start)
        let test_cases = vec![
            (
                block_height_started + epoch_duration,
                cur_epoch_num + 1,
                block_height_started + epoch_duration,
            ),
            (
                block_height_started + epoch_duration + epoch_duration / 2,
                cur_epoch_num + 1,
                block_height_started + epoch_duration,
            ),
            (
                block_height_started + epoch_duration * 4,
                cur_epoch_num + 4,
                block_height_started + epoch_duration * 4,
            ),
            (
                block_height_started + epoch_duration * 4 + epoch_duration / 2,
                cur_epoch_num + 4,
                block_height_started + epoch_duration * 4,
            ),
        ];

        for (height, expected_epoch_num, expected_block_start) in test_cases {
            let new_epoch = contract.current_epoch(height).unwrap();

            assert_eq!(new_epoch.epoch_num, expected_epoch_num);
            assert_eq!(new_epoch.block_height_started, expected_block_start);
        }
    }

    /// Tests that multiple participation events for the same contract within a given epoch are recorded correctly
    #[test]
    fn record_participation_multiple_events() {
        let cur_epoch_num = 1u64;
        let epoch_block_start = 250u64;
        let epoch_duration = 100u64;

        let mut contract = setup(cur_epoch_num, epoch_block_start, epoch_duration);

        let worker_contract = Addr::unchecked("some contract");

        let mut simulated_participation = HashMap::new();
        simulated_participation.insert(Addr::unchecked("worker_1"), 10);
        simulated_participation.insert(Addr::unchecked("worker_2"), 5);
        simulated_participation.insert(Addr::unchecked("worker_3"), 7);

        let event_count = 10;
        let mut cur_height = epoch_block_start;
        for i in 0..event_count {
            for (worker, part_count) in &simulated_participation {
                // simulates a worker participating in only part_count events
                if i < *part_count {
                    let event_id = i.to_string().try_into().unwrap();
                    contract
                        .record_participation(
                            event_id,
                            worker.clone(),
                            worker_contract.clone(),
                            cur_height,
                        )
                        .unwrap();
                }
            }
            cur_height = cur_height + 1;
        }

        let tally = contract
            .store
            .load_epoch_tally(worker_contract, cur_epoch_num)
            .unwrap();
        assert!(tally.is_some());

        let tally = tally.unwrap();
        assert_eq!(tally.event_count, event_count);
        assert_eq!(tally.participation.len(), simulated_participation.len());
        for (worker, part_count) in simulated_participation {
            assert_eq!(
                tally.participation.get(&worker.to_string()),
                Some(&part_count)
            );
        }
    }

    /// Tests that the participation event is recorded correctly when the event spans multiple epochs
    #[test]
    fn record_participation_epoch_boundary() {
        let starting_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;

        let mut contract = setup(starting_epoch_num, block_height_started, epoch_duration);

        let worker_contract = Addr::unchecked("some contract");

        let workers = vec![
            Addr::unchecked("worker_1"),
            Addr::unchecked("worker_2"),
            Addr::unchecked("worker_3"),
        ];
        // this is the height just before the next epoch starts
        let height_at_epoch_end = block_height_started + epoch_duration - 1;
        // workers participate in consecutive blocks
        for (i, workers) in workers.iter().enumerate() {
            contract
                .record_participation(
                    "some event".to_string().try_into().unwrap(),
                    workers.clone(),
                    worker_contract.clone(),
                    height_at_epoch_end + i as u64,
                )
                .unwrap();
        }

        let cur_epoch = contract.current_epoch(height_at_epoch_end).unwrap();
        assert_ne!(starting_epoch_num + 1, cur_epoch.epoch_num);

        let tally = contract
            .store
            .load_epoch_tally(worker_contract.clone(), starting_epoch_num)
            .unwrap();
        assert!(tally.is_some());

        let tally = tally.unwrap();

        assert_eq!(tally.event_count, 1);
        assert_eq!(tally.participation.len(), workers.len());
        for w in workers {
            assert_eq!(tally.participation.get(&w.to_string()), Some(&1));
        }

        let tally = contract
            .store
            .load_epoch_tally(worker_contract, starting_epoch_num + 1)
            .unwrap();
        assert!(tally.is_none());
    }

    /// Tests that participation events for different contracts are recorded correctly
    #[test]
    fn record_participation_multiple_contracts() {
        let cur_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;

        let mut contract = setup(cur_epoch_num, block_height_started, epoch_duration);

        let mut simulated_participation = HashMap::new();
        simulated_participation.insert(
            Addr::unchecked("worker_1"),
            (Addr::unchecked("contract_1"), 3),
        );
        simulated_participation.insert(
            Addr::unchecked("worker_2"),
            (Addr::unchecked("contract_2"), 4),
        );
        simulated_participation.insert(
            Addr::unchecked("worker_3"),
            (Addr::unchecked("contract_3"), 2),
        );

        for (worker, (worker_contract, events_participated)) in &simulated_participation {
            for i in 0..*events_participated {
                let event_id = i.to_string().try_into().unwrap();
                contract
                    .record_participation(
                        event_id,
                        worker.clone(),
                        worker_contract.clone(),
                        block_height_started,
                    )
                    .unwrap();
            }
        }
        for (worker, (worker_contract, events_participated)) in simulated_participation {
            let tally = contract
                .store
                .load_epoch_tally(worker_contract.clone(), cur_epoch_num)
                .unwrap();

            assert!(tally.is_some());
            let tally = tally.unwrap();

            assert_eq!(tally.event_count, events_participated);
            assert_eq!(tally.participation.len(), 1);
            assert_eq!(
                tally.participation.get(&worker.to_string()),
                Some(&events_participated)
            );
        }
    }
    /// Test that rewards parameters are updated correctly. In this test we don't change the epoch duration, so
    /// that computation of the current epoch is unaffected.
    #[test]
    fn update_params() {
        let initial_epoch_num = 1u64;
        let initial_epoch_start = 250u64;
        let initial_rewards_per_epoch = 100u128;
        let initial_participation_threshold = (1, 2);
        let epoch_duration = 100u64;
        let mut contract = setup_with_params(
            initial_epoch_num,
            initial_epoch_start,
            epoch_duration,
            initial_rewards_per_epoch,
            initial_participation_threshold,
        );

        // simulate the below tests running at this block height
        let cur_height = initial_epoch_start + epoch_duration * 10 + 2;

        let new_params = Params {
            rewards_per_epoch: cosmwasm_std::Uint128::from(initial_rewards_per_epoch + 100)
                .try_into()
                .unwrap(),
            participation_threshold: (Uint64::new(2), Uint64::new(3)).try_into().unwrap(),
            epoch_duration: epoch_duration.try_into().unwrap(), // keep this the same to not affect epoch computation
        };

        // the epoch shouldn't change when the params are updated, since we are not changing the epoch duration
        let expected_epoch = contract.current_epoch(cur_height).unwrap();

        contract
            .update_params(
                new_params.clone(),
                cur_height,
                contract.config.governance.clone(),
            )
            .unwrap();
        let stored = contract.store.load_params();
        assert_eq!(stored.params, new_params);

        // current epoch shouldn't have changed
        let cur_epoch = contract.current_epoch(cur_height).unwrap();
        assert_eq!(expected_epoch.epoch_num, cur_epoch.epoch_num);
        assert_eq!(
            expected_epoch.block_height_started,
            cur_epoch.block_height_started
        );

        // last updated should be the current epoch
        assert_eq!(stored.last_updated, cur_epoch);
    }

    /// Test that rewards parameters cannot be updated by an address other than governance
    #[test]
    fn update_params_unauthorized() {
        let initial_epoch_num = 1u64;
        let initial_epoch_start = 250u64;
        let epoch_duration = 100u64;
        let mut contract = setup(initial_epoch_num, initial_epoch_start, epoch_duration);

        let new_params = Params {
            rewards_per_epoch: cosmwasm_std::Uint128::from(100u128).try_into().unwrap(),
            participation_threshold: (Uint64::new(2), Uint64::new(3)).try_into().unwrap(),
            epoch_duration: epoch_duration.try_into().unwrap(),
        };

        let res = contract.update_params(
            new_params.clone(),
            initial_epoch_start,
            Addr::unchecked("some non governance address"),
        );
        assert!(res.is_err());
        assert_eq!(
            res.unwrap_err().current_context(),
            &ContractError::Unauthorized
        );
    }

    /// Test extending the epoch duration. This should not change the current epoch
    #[test]
    fn extend_epoch_duration() {
        let initial_epoch_num = 1u64;
        let initial_epoch_start = 250u64;
        let initial_epoch_duration = 100u64;
        let mut contract = setup(
            initial_epoch_num,
            initial_epoch_start,
            initial_epoch_duration,
        );

        // simulate the tests running after 5 epochs have passed
        let epochs_elapsed = 5;
        let cur_height = initial_epoch_start + initial_epoch_duration * epochs_elapsed + 10; // add 10 here just to be a little past the epoch boundary

        // epoch shouldn't change if we are extending the duration
        let epoch_prior_to_update = contract.current_epoch(cur_height).unwrap();

        let new_epoch_duration = initial_epoch_duration * 2;
        let new_params = Params {
            epoch_duration: (new_epoch_duration).try_into().unwrap(),
            ..contract.store.load_params().params // keep everything besides epoch duration the same
        };

        contract
            .update_params(
                new_params.clone(),
                cur_height,
                contract.config.governance.clone(),
            )
            .unwrap();

        // current epoch shouldn't change
        let epoch = contract.current_epoch(cur_height).unwrap();
        assert_eq!(epoch, epoch_prior_to_update);

        // we increased the epoch duration, so adding the initial epoch duration should leave us in the same epoch
        let epoch = contract
            .current_epoch(cur_height + initial_epoch_duration)
            .unwrap();
        assert_eq!(epoch, epoch_prior_to_update);

        // check that we can correctly compute the start of the next epoch
        let next_epoch = contract
            .current_epoch(cur_height + new_epoch_duration)
            .unwrap();
        assert_eq!(next_epoch.epoch_num, epoch_prior_to_update.epoch_num + 1);
        assert_eq!(
            next_epoch.block_height_started,
            epoch_prior_to_update.block_height_started + new_epoch_duration
        );
    }

    /// Test shortening the epoch duration. This test shortens the epoch duration such that the current epoch doesn't change
    /// (i.e. we are 10 blocks into the epoch, and we shorten the duration from 100 to 50)
    #[test]
    fn shorten_epoch_duration_same_epoch() {
        let initial_epoch_num = 1u64;
        let initial_epoch_start = 256u64;
        let initial_epoch_duration = 100u64;
        let mut contract = setup(
            initial_epoch_num,
            initial_epoch_start,
            initial_epoch_duration,
        );

        // simulate the tests running after 10 epochs have passed
        let epochs_elapsed = 10;
        let cur_height = initial_epoch_start + initial_epoch_duration * epochs_elapsed;

        let new_epoch_duration = initial_epoch_duration / 2;
        let epoch_prior_to_update = contract.current_epoch(cur_height).unwrap();
        // we are shortening the epoch, but not so much it causes the epoch number to change. We want to remain in the same epoch
        assert!(cur_height - epoch_prior_to_update.block_height_started < new_epoch_duration);

        let new_params = Params {
            epoch_duration: new_epoch_duration.try_into().unwrap(),
            ..contract.store.load_params().params
        };
        contract
            .update_params(
                new_params.clone(),
                cur_height,
                contract.config.governance.clone(),
            )
            .unwrap();

        // current epoch shouldn't have changed
        let epoch = contract.current_epoch(cur_height).unwrap();
        assert_eq!(epoch_prior_to_update, epoch);

        // adding the new epoch duration should increase the epoch number by 1
        let epoch = contract
            .current_epoch(cur_height + new_epoch_duration)
            .unwrap();
        assert_eq!(epoch.epoch_num, epoch_prior_to_update.epoch_num + 1);
        assert_eq!(
            epoch.block_height_started,
            epoch_prior_to_update.block_height_started + new_epoch_duration
        );
    }

    /// Tests shortening the epoch duration such that the current epoch does change
    /// (i.e. we are 50 blocks into the epoch, and we shorten the duration to 10 blocks)
    #[test]
    fn shorten_epoch_duration_diff_epoch() {
        let initial_epoch_num = 1u64;
        let initial_epoch_start = 250u64;
        let initial_epoch_duration = 100u64;
        let mut contract = setup(
            initial_epoch_num,
            initial_epoch_start,
            initial_epoch_duration,
        );

        // simulate running the test after 100 epochs have elapsed
        let epochs_elapsed = 100;
        let new_epoch_duration = 10;

        // simulate progressing far enough into the epoch such that shortening the epoch duration would change the epoch
        let cur_height =
            initial_epoch_start + initial_epoch_duration * epochs_elapsed + new_epoch_duration * 2;
        let epoch_prior_to_update = contract.current_epoch(cur_height).unwrap();

        let new_params = Params {
            epoch_duration: 10.try_into().unwrap(),
            ..contract.store.load_params().params
        };
        contract
            .update_params(
                new_params.clone(),
                cur_height,
                contract.config.governance.clone(),
            )
            .unwrap();

        // should be in new epoch now
        let epoch = contract.current_epoch(cur_height).unwrap();
        assert_eq!(epoch.epoch_num, epoch_prior_to_update.epoch_num + 1);
        assert_eq!(epoch.block_height_started, cur_height);

        // moving forward the new epoch duration # of blocks should increment the epoch
        let epoch = contract
            .current_epoch(cur_height + new_epoch_duration)
            .unwrap();
        assert_eq!(epoch.epoch_num, epoch_prior_to_update.epoch_num + 2);
        assert_eq!(epoch.block_height_started, cur_height + new_epoch_duration);
    }

    /// Tests that rewards are added correctly to a single contract
    #[test]
    fn added_rewards_should_be_reflected_in_rewards_pool() {
        let cur_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;

        let mut contract = setup(cur_epoch_num, block_height_started, epoch_duration);
        let worker_contract = Addr::unchecked("some contract");
        let pool = contract
            .store
            .load_rewards_pool(worker_contract.clone())
            .unwrap();
        assert!(pool.balance.is_zero());

        let initial_amount = Uint128::from(100u128);
        contract
            .add_rewards(worker_contract.clone(), initial_amount.try_into().unwrap())
            .unwrap();

        let pool = contract
            .store
            .load_rewards_pool(worker_contract.clone())
            .unwrap();
        assert_eq!(pool.balance, initial_amount);

        let added_amount = Uint128::from(500u128);
        contract
            .add_rewards(worker_contract.clone(), added_amount.try_into().unwrap())
            .unwrap();

        let pool = contract.store.load_rewards_pool(worker_contract).unwrap();
        assert_eq!(pool.balance, initial_amount + added_amount);
    }

    /// Tests that rewards are added correctly with multiple contracts
    #[test]
    fn added_rewards_for_multiple_contracts_should_be_reflected_in_multiple_pools() {
        let cur_epoch_num = 1u64;
        let block_height_started = 250u64;
        let epoch_duration = 100u64;

        let mut contract = setup(cur_epoch_num, block_height_started, epoch_duration);
        // a vector of (worker contract, rewards amounts) pairs
        let test_data = vec![
            (Addr::unchecked("contract_1"), vec![100, 200, 50]),
            (Addr::unchecked("contract_2"), vec![25, 500, 70]),
            (Addr::unchecked("contract_3"), vec![1000, 500, 2000]),
        ];

        for (worker_contract, rewards) in &test_data {
            for amount in rewards {
                contract
                    .add_rewards(
                        worker_contract.clone(),
                        cosmwasm_std::Uint128::from(*amount).try_into().unwrap(),
                    )
                    .unwrap();
            }
        }

        for (worker_contract, rewards) in test_data {
            let pool = contract.store.load_rewards_pool(worker_contract).unwrap();
            assert_eq!(
                pool.balance,
                cosmwasm_std::Uint128::from(rewards.iter().sum::<u128>())
            );
        }
    }

    /// Tests that rewards are distributed correctly based on participation
    #[test]
    fn distribute_rewards() {
        let cur_epoch_num = 0u64;
        let block_height_started = 0u64;
        let epoch_duration = 1000u64;
        let rewards_per_epoch = 100u128;
        let participation_threshold = (2, 3);

        let mut contract = setup_with_params(
            cur_epoch_num,
            block_height_started,
            epoch_duration,
            rewards_per_epoch,
            participation_threshold,
        );
        let worker1 = Addr::unchecked("worker1");
        let worker2 = Addr::unchecked("worker2");
        let worker3 = Addr::unchecked("worker3");
        let worker4 = Addr::unchecked("worker4");
        let epoch_count = 4;
        // Simulate 4 epochs worth of events with 4 workers
        // Each epoch has 3 possible events to participate in
        // The integer values represent which events a specific worker participated in during that epoch
        // Events in different epochs are considered distinct; we append the epoch number when generating the event id
        // The below participation corresponds to the following:
        // 2 workers rewarded in epoch 0, no workers in epoch 1 (no events in that epoch), no workers in epoch 2 (but still some events), and then 4 (all) workers in epoch 3
        let worker_participation_per_epoch = HashMap::from([
            (
                worker1.clone(),
                [vec![1, 2, 3], vec![], vec![1], vec![2, 3]], // represents the worker participated in events 1,2 and 3 in epoch 0, no events in epoch 1, event 1 in epoch 2, and events 2 and 3 in epoch 3
            ),
            (worker2.clone(), [vec![], vec![], vec![2], vec![1, 2, 3]]),
            (worker3.clone(), [vec![1, 2], vec![], vec![3], vec![1, 2]]),
            (worker4.clone(), [vec![1], vec![], vec![2], vec![2, 3]]),
        ]);
        // The expected rewards per worker over all 4 epochs. Based on the above participation
        let expected_rewards_per_worker: HashMap<Addr, u128> = HashMap::from([
            (
                worker1.clone(),
                rewards_per_epoch / 2 + rewards_per_epoch / 4,
            ),
            (worker2.clone(), rewards_per_epoch / 4),
            (
                worker3.clone(),
                rewards_per_epoch / 2 + rewards_per_epoch / 4,
            ),
            (worker4.clone(), rewards_per_epoch / 4),
        ]);
        let contract_addr = Addr::unchecked("worker_contract");

        for (worker, events_participated) in worker_participation_per_epoch.clone() {
            for epoch in 0..epoch_count {
                for event in &events_participated[epoch] {
                    let event_id = event.to_string() + &epoch.to_string() + "event";
                    let _ = contract.record_participation(
                        event_id.clone().try_into().unwrap(),
                        worker.clone(),
                        contract_addr.clone(),
                        block_height_started + epoch as u64 * epoch_duration,
                    );
                }
            }
        }

        // we add 2 epochs worth of rewards. There were 2 epochs of participation, but only 2 epochs where rewards should be given out
        // This tests we are accounting correctly, and only removing from the pool when we actually give out rewards
        let rewards_added = 2 * rewards_per_epoch;
        let _ = contract.add_rewards(
            contract_addr.clone(),
            Uint128::from(rewards_added).try_into().unwrap(),
        );

        let rewards_claimed = contract
            .distribute_rewards(
                contract_addr,
                block_height_started + epoch_duration * (epoch_count + 2) as u64,
                None,
            )
            .unwrap();

        assert_eq!(rewards_claimed.len(), worker_participation_per_epoch.len());
        for (worker, rewards) in expected_rewards_per_worker {
            assert!(rewards_claimed.contains_key(&worker));
            assert_eq!(rewards_claimed.get(&worker), Some(&Uint128::from(rewards)));
        }
    }

    /// Tests that rewards are distributed correctly for a specified number of epochs, and that pagination works correctly
    #[test]
    fn distribute_rewards_specify_epoch_count() {
        let cur_epoch_num = 0u64;
        let block_height_started = 0u64;
        let epoch_duration = 1000u64;
        let rewards_per_epoch = 100u128;
        let participation_threshold = (1, 2);

        let mut contract = setup_with_params(
            cur_epoch_num,
            block_height_started,
            epoch_duration,
            rewards_per_epoch,
            participation_threshold,
        );
        let worker = Addr::unchecked("worker");
        let contract_addr = Addr::unchecked("worker_contract");

        for height in block_height_started..block_height_started + epoch_duration * 9 {
            let event_id = height.to_string() + "event";
            let _ = contract.record_participation(
                event_id.try_into().unwrap(),
                worker.clone(),
                contract_addr.clone(),
                height,
            );
        }

        let rewards_added = 1000u128;
        let _ = contract.add_rewards(
            contract_addr.clone(),
            Uint128::from(rewards_added).try_into().unwrap(),
        );

        // this puts us in epoch 10
        let cur_height = block_height_started + epoch_duration * 9;
        let total_epochs_with_rewards = (cur_height / epoch_duration) - 1;

        // distribute 5 epochs worth of rewards
        let epochs_to_process = 5;
        let rewards_claimed = contract
            .distribute_rewards(contract_addr.clone(), cur_height, Some(epochs_to_process))
            .unwrap();
        assert_eq!(rewards_claimed.len(), 1);
        assert!(rewards_claimed.contains_key(&worker));
        assert_eq!(
            rewards_claimed.get(&worker),
            Some(&(rewards_per_epoch * epochs_to_process as u128).into())
        );

        // distribute the remaining epochs worth of rewards
        let rewards_claimed = contract
            .distribute_rewards(contract_addr.clone(), cur_height, None)
            .unwrap();
        assert_eq!(rewards_claimed.len(), 1);
        assert!(rewards_claimed.contains_key(&worker));
        assert_eq!(
            rewards_claimed.get(&worker),
            Some(
                &(rewards_per_epoch * (total_epochs_with_rewards - epochs_to_process) as u128)
                    .into()
            )
        );
    }

    /// Tests that we do not distribute rewards for a given epoch until two epochs later
    #[test]
    fn distribute_rewards_too_early() {
        let cur_epoch_num = 0u64;
        let block_height_started = 0u64;
        let epoch_duration = 1000u64;
        let rewards_per_epoch = 100u128;
        let participation_threshold = (8, 10);

        let mut contract = setup_with_params(
            cur_epoch_num,
            block_height_started,
            epoch_duration,
            rewards_per_epoch,
            participation_threshold,
        );
        let worker = Addr::unchecked("worker");
        let contract_addr = Addr::unchecked("worker_contract");

        let _ = contract.record_participation(
            "event".try_into().unwrap(),
            worker.clone(),
            contract_addr.clone(),
            block_height_started,
        );

        let rewards_added = 1000u128;
        let _ = contract.add_rewards(
            contract_addr.clone(),
            Uint128::from(rewards_added).try_into().unwrap(),
        );

        // too early, still in the same epoch
        let err = contract
            .distribute_rewards(contract_addr.clone(), block_height_started, None)
            .unwrap_err();
        assert_eq!(err.current_context(), &ContractError::NoRewardsToDistribute);

        // next epoch, but still too early to claim rewards
        let err = contract
            .distribute_rewards(
                contract_addr.clone(),
                block_height_started + epoch_duration,
                None,
            )
            .unwrap_err();
        assert_eq!(err.current_context(), &ContractError::NoRewardsToDistribute);

        // can claim now, two epochs after participation
        let rewards_claimed = contract
            .distribute_rewards(
                contract_addr.clone(),
                block_height_started + epoch_duration * 2,
                None,
            )
            .unwrap();
        assert_eq!(rewards_claimed.len(), 1);

        // should error if we try again
        let err = contract
            .distribute_rewards(
                contract_addr,
                block_height_started + epoch_duration * 2,
                None,
            )
            .unwrap_err();
        assert_eq!(err.current_context(), &ContractError::NoRewardsToDistribute);
    }

    /// Tests that an error is returned from distribute_rewards when the rewards pool balance is too low to distribute rewards,
    /// and that rewards can later be added and subsequently claimed
    #[test]
    fn distribute_rewards_low_balance() {
        let cur_epoch_num = 0u64;
        let block_height_started = 0u64;
        let epoch_duration = 1000u64;
        let rewards_per_epoch = 100u128;
        let participation_threshold = (8, 10);

        let mut contract = setup_with_params(
            cur_epoch_num,
            block_height_started,
            epoch_duration,
            rewards_per_epoch,
            participation_threshold,
        );
        let worker = Addr::unchecked("worker");
        let contract_addr = Addr::unchecked("worker_contract");

        let _ = contract.record_participation(
            "event".try_into().unwrap(),
            worker.clone(),
            contract_addr.clone(),
            block_height_started,
        );

        // rewards per epoch is 100, we only add 10
        let rewards_added = 10u128;
        let _ = contract.add_rewards(
            contract_addr.clone(),
            Uint128::from(rewards_added).try_into().unwrap(),
        );

        let err = contract
            .distribute_rewards(
                contract_addr.clone(),
                block_height_started + epoch_duration * 2,
                None,
            )
            .unwrap_err();
        assert_eq!(
            err.current_context(),
            &ContractError::PoolBalanceInsufficient
        );
        // add some more rewards
        let rewards_added = 90u128;
        let _ = contract.add_rewards(
            contract_addr.clone(),
            Uint128::from(rewards_added).try_into().unwrap(),
        );

        let result = contract.distribute_rewards(
            contract_addr,
            block_height_started + epoch_duration * 2,
            None,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }

    /// Tests that an error is returned from distribute_rewards when trying to claim rewards for the same epoch more than once
    #[test]
    fn distribute_rewards_already_distributed() {
        let cur_epoch_num = 0u64;
        let block_height_started = 0u64;
        let epoch_duration = 1000u64;
        let rewards_per_epoch = 100u128;
        let participation_threshold = (8, 10);

        let mut contract = setup_with_params(
            cur_epoch_num,
            block_height_started,
            epoch_duration,
            rewards_per_epoch,
            participation_threshold,
        );
        let worker = Addr::unchecked("worker");
        let contract_addr = Addr::unchecked("worker_contract");

        let _ = contract.record_participation(
            "event".try_into().unwrap(),
            worker.clone(),
            contract_addr.clone(),
            block_height_started,
        );

        let rewards_added = 1000u128;
        let _ = contract.add_rewards(
            contract_addr.clone(),
            Uint128::from(rewards_added).try_into().unwrap(),
        );

        let rewards_claimed = contract
            .distribute_rewards(
                contract_addr.clone(),
                block_height_started + epoch_duration * 2,
                None,
            )
            .unwrap();
        assert_eq!(rewards_claimed.len(), 1);

        // try to claim again, shouldn't get an error
        let err = contract
            .distribute_rewards(
                contract_addr,
                block_height_started + epoch_duration * 2,
                None,
            )
            .unwrap_err();
        assert_eq!(err.current_context(), &ContractError::NoRewardsToDistribute);
    }

    fn create_contract(
        params_store: Arc<RwLock<StoredParams>>,
        events_store: Arc<RwLock<HashMap<(String, Addr), Event>>>,
        tally_store: Arc<RwLock<HashMap<(Addr, u64), EpochTally>>>,
        rewards_store: Arc<RwLock<HashMap<Addr, RewardsPool>>>,
        watermark_store: Arc<RwLock<HashMap<Addr, u64>>>,
    ) -> Contract<state::MockStore> {
        let mut store = state::MockStore::new();
        let params_store_cloned = params_store.clone();
        store
            .expect_load_params()
            .returning(move || params_store_cloned.read().unwrap().clone());
        store.expect_save_params().returning(move |new_params| {
            let mut params_store = params_store.write().unwrap();
            *params_store = new_params.clone();
            Ok(())
        });
        let events_store_cloned = events_store.clone();
        store.expect_load_event().returning(move |id, contract| {
            let events_store = events_store_cloned.read().unwrap();
            Ok(events_store.get(&(id, contract)).cloned())
        });
        store.expect_save_event().returning(move |event| {
            let mut events_store = events_store.write().unwrap();
            events_store.insert(
                (event.event_id.clone().into(), event.contract.clone()),
                event.clone(),
            );
            Ok(())
        });
        let tally_store_cloned = tally_store.clone();
        store
            .expect_load_epoch_tally()
            .returning(move |contract, epoch_num| {
                let tally_store = tally_store_cloned.read().unwrap();
                Ok(tally_store.get(&(contract, epoch_num)).cloned())
            });
        store.expect_save_epoch_tally().returning(move |tally| {
            let mut tally_store = tally_store.write().unwrap();
            tally_store.insert(
                (tally.contract.clone(), tally.epoch.epoch_num.clone()),
                tally.clone(),
            );
            Ok(())
        });

        let rewards_store_cloned = rewards_store.clone();
        store.expect_load_rewards_pool().returning(move |contract| {
            let rewards_store = rewards_store_cloned.read().unwrap();
            Ok(rewards_store
                .get(&contract)
                .cloned()
                .unwrap_or(RewardsPool {
                    contract,
                    balance: Uint128::zero(),
                }))
        });
        store.expect_save_rewards_pool().returning(move |pool| {
            let mut rewards_store = rewards_store.write().unwrap();
            rewards_store.insert(pool.contract.clone(), pool.clone());
            Ok(())
        });

        let watermark_store_cloned = watermark_store.clone();
        store
            .expect_load_rewards_watermark()
            .returning(move |contract| {
                let watermark_store = watermark_store_cloned.read().unwrap();
                Ok(watermark_store.get(&contract).cloned())
            });
        store
            .expect_save_rewards_watermark()
            .returning(move |contract, epoch_num| {
                let mut watermark_store = watermark_store.write().unwrap();
                watermark_store.insert(contract, epoch_num);
                Ok(())
            });
        Contract {
            store,
            config: Config {
                governance: Addr::unchecked("governance"),
                rewards_denom: "AXL".to_string(),
            },
        }
    }

    fn setup_with_stores(
        params_store: Arc<RwLock<StoredParams>>,
        events_store: Arc<RwLock<HashMap<(String, Addr), Event>>>,
        tally_store: Arc<RwLock<HashMap<(Addr, u64), EpochTally>>>,
        rewards_store: Arc<RwLock<HashMap<Addr, RewardsPool>>>,
        watermark_store: Arc<RwLock<HashMap<Addr, u64>>>,
    ) -> Contract<state::MockStore> {
        create_contract(
            params_store,
            events_store,
            tally_store,
            rewards_store,
            watermark_store,
        )
    }

    fn setup_with_params(
        cur_epoch_num: u64,
        block_height_started: u64,
        epoch_duration: u64,
        rewards_per_epoch: u128,
        participation_threshold: (u64, u64),
    ) -> Contract<state::MockStore> {
        let rewards_per_epoch: nonempty::Uint128 = cosmwasm_std::Uint128::from(rewards_per_epoch)
            .try_into()
            .unwrap();
        let current_epoch = Epoch {
            epoch_num: cur_epoch_num,
            block_height_started,
        };

        let stored_params = StoredParams {
            params: Params {
                participation_threshold: participation_threshold.try_into().unwrap(),
                epoch_duration: epoch_duration.try_into().unwrap(),
                rewards_per_epoch,
            },
            last_updated: current_epoch.clone(),
        };
        let stored_params = Arc::new(RwLock::new(stored_params));
        let rewards_store = Arc::new(RwLock::new(HashMap::new()));
        let events_store = Arc::new(RwLock::new(HashMap::new()));
        let tally_store = Arc::new(RwLock::new(HashMap::new()));
        let watermark_store = Arc::new(RwLock::new(HashMap::new()));
        setup_with_stores(
            stored_params,
            events_store,
            tally_store,
            rewards_store,
            watermark_store,
        )
    }

    fn setup(
        cur_epoch_num: u64,
        block_height_started: u64,
        epoch_duration: u64,
    ) -> Contract<state::MockStore> {
        let participation_threshold = (1, 2);
        let rewards_per_epoch = 100u128;
        setup_with_params(
            cur_epoch_num,
            block_height_started,
            epoch_duration,
            rewards_per_epoch,
            participation_threshold,
        )
    }
}
