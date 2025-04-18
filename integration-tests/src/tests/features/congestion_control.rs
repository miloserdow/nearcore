use assert_matches::assert_matches;
use near_chain::Provenance;
use near_chain_configs::Genesis;
use near_client::ProcessTxResponse;
use near_crypto::{InMemorySigner, KeyType, PublicKey, Signer};
use near_epoch_manager::shard_assignment::account_id_to_shard_id;
use near_o11y::testonly::init_test_logger;
use near_parameters::{RuntimeConfig, RuntimeConfigStore};
use near_primitives::account::id::AccountId;
use near_primitives::congestion_info::{CongestionControl, CongestionInfo};
use near_primitives::errors::{
    ActionErrorKind, FunctionCallError, InvalidTxError, TxExecutionError,
};
use near_primitives::hash::CryptoHash;
use near_primitives::shard_layout::ShardLayout;
use near_primitives::sharding::{ShardChunk, ShardChunkHeader};
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::{EpochId, ShardId};
use near_primitives::version::{PROTOCOL_VERSION, ProtocolFeature};
use near_primitives::views::FinalExecutionStatus;
use near_vm_runner::logic::ProtocolVersion;
use node_runtime::bootstrap_congestion_info;
use std::sync::Arc;

use crate::env::nightshade_setup::TestEnvNightshadeSetupExt;
use crate::env::test_env::TestEnv;
use crate::env::test_env_builder::TestEnvBuilder;

const ACCOUNT_PARENT_ID: &str = "near";
const CONTRACT_ID: &str = "contract.near";

fn get_runtime_config(
    config_store: &RuntimeConfigStore,
    protocol_version: ProtocolVersion,
) -> Arc<near_parameters::RuntimeConfig> {
    let mut config = config_store.get_config(protocol_version).clone();
    let mut_config = Arc::make_mut(&mut config);

    set_wasm_cost(mut_config);

    config
}

// Make 1 wasm op cost ~4 GGas, to let "loop_forever" finish more quickly.
fn set_wasm_cost(config: &mut RuntimeConfig) {
    let wasm_config = Arc::make_mut(&mut config.wasm_config);
    wasm_config.regular_op_cost = u32::MAX;
}

// Set the default congestion control parameters for the given runtime config.
// This is important to prevent needing to fix the congestion control tests
// every time the parameters are updated.
fn set_default_congestion_control(config_store: &RuntimeConfigStore, config: &mut RuntimeConfig) {
    let cc_protocol_version = ProtocolFeature::CongestionControl.protocol_version();
    let cc_config = get_runtime_config(&config_store, cc_protocol_version);
    config.congestion_control_config = cc_config.congestion_control_config;
}

/// Set up the test runtime with the given protocol version and runtime configs.
/// The test version of runtime has custom gas cost.
fn setup_test_runtime(_sender_id: AccountId, protocol_version: ProtocolVersion) -> TestEnv {
    let accounts = TestEnvBuilder::make_accounts(1);
    let mut genesis = Genesis::test_sharded_new_version(accounts, 1, vec![1, 1, 1, 1]);
    genesis.config.epoch_length = 10;
    genesis.config.protocol_version = protocol_version;

    // Chain must be sharded to test cross-shard congestion control.
    genesis.config.shard_layout = ShardLayout::multi_shard(4, 3);

    let config_store = RuntimeConfigStore::new(None);
    let mut config = RuntimeConfig::test_protocol_version(protocol_version);
    set_wasm_cost(&mut config);
    set_default_congestion_control(&config_store, &mut config);

    let runtime_configs = vec![RuntimeConfigStore::with_one_config(config)];
    TestEnv::builder(&genesis.config)
        .nightshade_runtimes_with_runtime_config_store(&genesis, runtime_configs)
        .build()
}

/// Set up the real runtime with the given protocol version and runtime configs.
/// This runtime is suitable for testing protocol upgrade and the migration from
/// Receipt to StateStoredReceipt.
fn setup_real_runtime(sender_id: AccountId, protocol_version: ProtocolVersion) -> TestEnv {
    let mut genesis = Genesis::test_sharded_new_version(vec![sender_id], 1, vec![1, 1, 1, 1]);
    genesis.config.epoch_length = 10;
    genesis.config.protocol_version = protocol_version;

    // Chain must be sharded to test cross-shard congestion control.
    genesis.config.shard_layout = ShardLayout::multi_shard(4, 3);

    // Get the runtime configs for before and after the protocol upgrade.
    let config_store = RuntimeConfigStore::new(None);
    let pre_config = get_runtime_config(&config_store, protocol_version);
    let mut post_config = get_runtime_config(&config_store, PROTOCOL_VERSION);

    // Use the original congestion control parameters for the post config.
    let post_runtime_config = Arc::get_mut(&mut post_config).unwrap();
    set_default_congestion_control(&config_store, post_runtime_config);

    // Checking the migration from Receipt to StateStoredReceipt requires the
    // relevant config to be disabled before the protocol upgrade and enabled
    // after the protocol upgrade.
    assert!(false == pre_config.use_state_stored_receipt);
    assert!(true == post_config.use_state_stored_receipt);

    let runtime_configs = vec![RuntimeConfigStore::new_custom(
        [(protocol_version, pre_config), (PROTOCOL_VERSION, post_config)].into_iter().collect(),
    )];

    TestEnv::builder(&genesis.config)
        .nightshade_runtimes_with_runtime_config_store(&genesis, runtime_configs)
        .build()
}

fn setup_account(
    env: &mut TestEnv,
    nonce: &mut u64,
    account_id: &AccountId,
    account_parent_id: &AccountId,
) {
    let block = env.clients[0].chain.get_head_block().unwrap();
    let block_hash = block.hash();

    let signer_id = account_parent_id.clone();
    let signer = InMemorySigner::test_signer(&signer_id);

    let public_key = PublicKey::from_seed(KeyType::ED25519, account_id.as_str());
    let amount = 10 * 10u128.pow(24);

    *nonce += 1;
    let create_account_tx = SignedTransaction::create_account(
        *nonce,
        signer_id,
        account_id.clone(),
        amount,
        public_key,
        &signer,
        *block_hash,
    );

    env.execute_tx(create_account_tx).unwrap().assert_success();
}

/// Set up the RS-Contract, which includes useful functions, such as
/// `loop_forever`.
///
/// This function also advances the chain to complete the deployment and checks
/// it can be called successfully.
fn setup_contract(env: &mut TestEnv, nonce: &mut u64) {
    let block = env.clients[0].chain.get_head_block().unwrap();
    let contract = near_test_contracts::congestion_control_test_contract();

    let signer_id: AccountId = ACCOUNT_PARENT_ID.parse().unwrap();
    let signer = InMemorySigner::test_signer(&signer_id);

    *nonce += 1;
    let create_contract_tx = SignedTransaction::create_contract(
        *nonce,
        signer_id,
        CONTRACT_ID.parse().unwrap(),
        contract.to_vec(),
        10 * 10u128.pow(24),
        PublicKey::from_seed(KeyType::ED25519, CONTRACT_ID),
        &signer,
        *block.hash(),
    );
    // this adds the tx to the pool and then produces blocks until the tx result is available
    env.execute_tx(create_contract_tx).unwrap().assert_success();

    // Test the function call works as expected, ending in a gas exceeded error.
    *nonce += 1;
    let block = env.clients[0].chain.get_head_block().unwrap();
    let fn_tx = new_fn_call_100tgas(nonce, &signer, *block.hash());
    let FinalExecutionStatus::Failure(TxExecutionError::ActionError(action_error)) =
        env.execute_tx(fn_tx).unwrap().status
    else {
        panic!("test setup error: should result in action error")
    };
    assert_eq!(
        action_error.kind,
        ActionErrorKind::FunctionCallError(FunctionCallError::ExecutionError(
            "Exceeded the prepaid gas.".to_owned()
        )),
        "test setup error: should result in gas exceeded error"
    );
}

/// Check that the congestion info is correctly bootstrapped, updated and
/// propagated from chunk extra to chunk header. If the
/// `check_congested_protocol_upgrade` flag is set check that the chain is under
/// congestion during the protocol upgrade.
fn check_congestion_info(env: &TestEnv, check_congested_protocol_upgrade: bool) {
    let client = &env.clients[0];
    let genesis_height = client.chain.genesis().height();
    let head_height = client.chain.head().unwrap().height;

    let mut check_congested_protocol_upgrade_done = false;

    let shard_layout = client.epoch_manager.get_shard_layout(&EpochId::default()).unwrap();
    let contract_id = CONTRACT_ID.parse().unwrap();
    let contract_shard_id = shard_layout.account_id_to_shard_id(&contract_id);

    for height in genesis_height..head_height + 1 {
        let block = client.chain.get_block_by_height(height);
        let Ok(block) = block else {
            continue;
        };

        let prev_hash = block.header().prev_hash();
        let epoch_id = client.epoch_manager.get_epoch_id(block.hash()).unwrap();
        let shard_layout = client.epoch_manager.get_shard_layout(&epoch_id).unwrap();
        let protocol_config = client.runtime_adapter.get_protocol_config(&epoch_id).unwrap();
        let runtime_config = protocol_config.runtime_config;

        for (shard_index, chunk) in block.chunks().iter_raw().enumerate() {
            let shard_id = shard_layout.get_shard_id(shard_index).unwrap();

            let prev_state_root = chunk.prev_state_root();

            let trie = client
                .chain
                .runtime_adapter
                .get_trie_for_shard(shard_id, prev_hash, prev_state_root, false)
                .unwrap();
            let mut computed_congestion_info =
                bootstrap_congestion_info(&trie, &runtime_config, shard_id).unwrap();

            tracing::info!(target: "test", ?epoch_id, ?height, ?shard_id, ?computed_congestion_info, "checking congestion info");

            let header_congestion_info = chunk.congestion_info();
            let Some(header_congestion_info) = header_congestion_info else {
                continue;
            };

            // Do not check the allowed shard as it's set separately from the
            // bootstrapping logic.
            computed_congestion_info.set_allowed_shard(header_congestion_info.allowed_shard());

            assert_eq!(
                header_congestion_info, computed_congestion_info,
                "congestion info mismatch at height {} for shard {}",
                height, shard_id
            );

            if shard_id == contract_shard_id
                && check_congested_protocol_upgrade
                && !check_congested_protocol_upgrade_done
            {
                let congestion_level = header_congestion_info
                    .localized_congestion_level(&runtime_config.congestion_control_config);
                assert!(
                    congestion_level > 0.0,
                    "the congestion level should be non-zero for the congested shard during protocol upgrade"
                );

                check_congested_protocol_upgrade_done = true;
            }
        }
    }
}

/// Simplest possible upgrade to new protocol with congestion control enabled,
/// no traffic at all.
#[test]
fn test_protocol_upgrade_simple() {
    init_test_logger();

    // The following only makes sense to test if the feature is enabled in the current build.
    if !ProtocolFeature::CongestionControl.enabled(PROTOCOL_VERSION) {
        return;
    }

    let mut env = setup_real_runtime(
        "test0".parse().unwrap(),
        ProtocolFeature::CongestionControl.protocol_version() - 1,
    );

    // Produce a few blocks to get out of initial state.
    let tip = env.clients[0].chain.head().unwrap();
    for i in 1..4 {
        env.produce_block(0, tip.height + i);
    }

    // Ensure we are still in the old version and no congestion info is shared.
    check_old_protocol(&env);

    env.upgrade_protocol_to_latest_version();

    // check we are in the new version
    assert!(ProtocolFeature::CongestionControl.enabled(env.get_head_protocol_version()));

    let block = env.clients[0].chain.get_head_block().unwrap();
    // check congestion info is available and represents "no congestion"
    let chunks = block.chunks();
    assert!(chunks.len() > 0);

    let config = head_congestion_control_config(&env);
    for chunk_header in chunks.iter_deprecated() {
        let congestion_info = chunk_header
            .congestion_info()
            .expect("chunk header must have congestion info after upgrade");
        let congestion_control = CongestionControl::new(config, congestion_info, 0);
        assert_eq!(congestion_control.congestion_level(), 0.0);
        assert!(congestion_control.shard_accepts_transactions().is_yes());
    }

    let check_congested_protocol_upgrade = false;
    check_congestion_info(&env, check_congested_protocol_upgrade);
}

fn head_congestion_control_config(
    env: &TestEnv,
) -> near_parameters::config::CongestionControlConfig {
    let block = env.clients[0].chain.get_head_block().unwrap();
    let runtime_config = env.get_runtime_config(0, *block.header().epoch_id());
    runtime_config.congestion_control_config
}

fn head_congestion_info(env: &TestEnv, shard_id: ShardId) -> CongestionInfo {
    let chunk = head_chunk_header(env, shard_id);
    chunk.congestion_info().unwrap()
}

fn head_chunk_header(env: &TestEnv, shard_id: ShardId) -> ShardChunkHeader {
    let block = env.clients[0].chain.get_head_block().unwrap();
    let chunks = block.chunks();

    let epoch_id = block.header().epoch_id();
    let shard_layout = env.clients[0].epoch_manager.get_shard_layout(epoch_id).unwrap();
    let shard_index = shard_layout.get_shard_index(shard_id).unwrap();
    chunks.get(shard_index).expect("chunk header must be available").clone()
}

fn head_chunk(env: &TestEnv, shard_id: ShardId) -> Arc<ShardChunk> {
    let chunk_header = head_chunk_header(&env, shard_id);
    env.clients[0].chain.get_chunk(&chunk_header.chunk_hash()).expect("chunk must be available")
}

#[test]
fn slow_test_protocol_upgrade_under_congestion() {
    init_test_logger();

    // The following only makes sense to test if the feature is enabled in the current build.
    if !ProtocolFeature::CongestionControl.enabled(PROTOCOL_VERSION) {
        return;
    }

    let sender_id: AccountId = "test0".parse().unwrap();
    let mut env = setup_real_runtime(
        sender_id.clone(),
        ProtocolFeature::CongestionControl.protocol_version() - 1,
    );

    // prepare a contract to call
    let mut nonce = 10;
    setup_contract(&mut env, &mut nonce);

    let signer = InMemorySigner::test_signer(&sender_id);
    // Now, congest the network with ~100 Pgas, enough to have some left after the protocol upgrade.
    let n = 1000;
    submit_n_100tgas_fns(&mut env, n, &mut nonce, &signer);

    // Allow transactions to enter the chain
    let tip = env.clients[0].chain.head().unwrap();
    for i in 1..3 {
        env.produce_block(0, tip.height + i);
    }

    // Ensure we are still in the old version and no congestion info is shared.
    check_old_protocol(&env);

    env.upgrade_protocol_to_latest_version();

    // check we are in the new version
    assert!(ProtocolFeature::CongestionControl.enabled(env.get_head_protocol_version()));
    // check congestion info is available
    let block = env.clients[0].chain.get_head_block().unwrap();
    let chunks = block.chunks();
    for chunk_header in chunks.iter_deprecated() {
        chunk_header
            .congestion_info()
            .expect("chunk header must have congestion info after upgrade");
    }
    let tip = env.clients[0].chain.head().unwrap();

    // Get the shard id and shard index of the contract account.
    // Please not that this is not updated and won't work for resharding.
    let epoch_id = tip.epoch_id;
    let shard_layout = env.clients[0].epoch_manager.get_shard_layout(&epoch_id).unwrap();
    let contract_shard_id = account_id_to_shard_id(
        env.clients[0].epoch_manager.as_ref(),
        &CONTRACT_ID.parse().unwrap(),
        &tip.epoch_id,
    )
    .unwrap();
    let contract_shard_index = shard_layout.get_shard_index(contract_shard_id).unwrap();

    // Check there is still congestion, which this test is all about.

    let congestion_info = head_congestion_info(&mut env, contract_shard_id);
    let config = head_congestion_control_config(&env);
    assert_eq!(
        congestion_info.localized_congestion_level(&config),
        1.0,
        "contract's shard should be fully congested"
    );

    // Also check that the congested shard is still making progress.
    let block = env.clients[0].produce_block(tip.height + 1).unwrap().unwrap();
    assert_eq!(block.header().chunk_mask()[contract_shard_index], true, "chunk isn't missing");
    let gas_used = block.chunks().get(contract_shard_index).unwrap().prev_gas_used();
    tracing::debug!(target: "test", "prev_gas_used: {}", gas_used);

    // The chunk should process at least 500TGas worth of receipts
    assert!(gas_used > 500_000_000_000_000);

    env.process_block(0, block, Provenance::PRODUCED);

    let check_congested_protocol_upgrade = true;
    check_congestion_info(&env, check_congested_protocol_upgrade);

    // Test the migration from Receipt to StateStoredReceipt

    // Wait until chain is no longer congested
    let tip = env.clients[0].chain.head().unwrap();
    for i in 1.. {
        let block = env.clients[0].produce_block(tip.height + i);
        let block = block.unwrap().unwrap();
        let gas_used = block.chunks().get(contract_shard_index).unwrap().prev_gas_used();

        env.process_block(0, block, Provenance::PRODUCED);

        if gas_used == 0 {
            break;
        }
    }

    // Submit some more transactions that should now be stored as StateStoredReceipts.
    let included = submit_n_100tgas_fns(&mut env, n, &mut nonce, &signer);
    assert!(included > 0);

    // Allow transactions to enter the chain and be processed. At this point the
    // receipts will be stored and retrieved using the StateStoredReceipt
    // structure.
    let tip = env.clients[0].chain.head().unwrap();
    for i in 1..10 {
        env.produce_block(0, tip.height + i);
    }

    // The summary may be incomplete because of GC.
    env.print_summary();
}

/// Check we are still in the old version and no congestion info is shared.
#[track_caller]
fn check_old_protocol(env: &TestEnv) {
    assert!(
        !ProtocolFeature::CongestionControl.enabled(env.get_head_protocol_version()),
        "test setup error: chain already updated to new protocol"
    );
    let block = env.clients[0].chain.get_head_block().unwrap();
    let chunks = block.chunks();
    assert!(chunks.len() > 0, "no chunks in block");
    for chunk_header in chunks.iter_deprecated() {
        assert!(
            chunk_header.congestion_info().is_none(),
            "old protocol should not have congestion info but found {:?}",
            chunk_header.congestion_info()
        );
    }
}

/// Create a function call that has 100 Tgas attached and will burn it all.
fn new_fn_call_100tgas(
    nonce_source: &mut u64,
    signer: &Signer,
    block_hash: CryptoHash,
) -> SignedTransaction {
    let hundred_tgas = 100 * 10u64.pow(12);
    let deposit = 0;
    let nonce = *nonce_source;
    *nonce_source += 1;
    SignedTransaction::call(
        nonce,
        signer.get_account_id(),
        CONTRACT_ID.parse().unwrap(),
        &signer,
        deposit,
        // easy way to burn all attached gas
        "loop_forever".to_owned(),
        vec![],
        hundred_tgas,
        block_hash,
    )
}

/// Create a dummy function call that is valid but allowed to fail when
/// executed. It has only 1 Tgas attached.
fn new_cheap_fn_call(
    nonce_source: &mut u64,
    signer: &Signer,
    receiver: AccountId,
    block_hash: CryptoHash,
) -> SignedTransaction {
    let one_tgas = 1 * 10u64.pow(12);
    let deposit = 0;
    let nonce = *nonce_source;
    *nonce_source += 1;
    SignedTransaction::call(
        nonce,
        signer.get_account_id(),
        receiver,
        &signer,
        deposit,
        "foo_does_not_exists".to_owned(),
        vec![],
        one_tgas,
        block_hash,
    )
}

/// Submit N transaction containing a function call action with 100 Tgas
/// attached that will all be burned when called.
fn submit_n_100tgas_fns(env: &mut TestEnv, n: u32, nonce: &mut u64, signer: &Signer) -> u32 {
    let mut included = 0;
    let block = env.clients[0].chain.get_head_block().unwrap();
    for _ in 0..n {
        let fn_tx = new_fn_call_100tgas(nonce, signer, *block.hash());
        // this only adds the tx to the pool, no chain progress is made
        let response = env.tx_request_handlers[0].process_tx(fn_tx, false, false);
        match response {
            ProcessTxResponse::ValidTx => {
                included += 1;
            }
            ProcessTxResponse::InvalidTx(InvalidTxError::ShardCongested { .. }) => (),
            other => panic!("unexpected result from submitting tx: {other:?}"),
        }
    }
    included
}

/// Submit N transaction containing a cheap function call action.
fn submit_n_cheap_fns(
    env: &mut TestEnv,
    n: u32,
    nonce: &mut u64,
    signer: &Signer,
    receiver: &AccountId,
) {
    let block = env.clients[0].chain.get_head_block().unwrap();
    for _ in 0..n {
        let fn_tx = new_cheap_fn_call(nonce, signer, receiver.clone(), *block.hash());
        // this only adds the tx to the pool, no chain progress is made
        let response = env.tx_request_handlers[0].process_tx(fn_tx, false, false);
        assert_eq!(response, ProcessTxResponse::ValidTx);
    }
}

/// Test that less gas is attributed to transactions when the local shard has
/// delayed receipts.
///
/// This test operates on one shard with transactions signed by the contract
/// itself, producing only local receipts. This should be enough to trigger the
/// linear interpolation between `max_tx_gas` and `min_tx_gas` and we want to
/// test that indeed local traffic is enough.
///
/// See [`test_transaction_limit_for_remote_congestion`] for a similar test but
/// with remote traffic.
#[test]
fn test_transaction_limit_for_local_congestion() {
    init_test_logger();

    if !ProtocolFeature::CongestionControl.enabled(PROTOCOL_VERSION) {
        return;
    }
    // Fix the initial configuration of congestion control for the tests.
    let protocol_version = ProtocolFeature::CongestionControl.protocol_version();
    // We don't want to go into the TX rejection limit in this test.
    let upper_limit_congestion = UpperLimitCongestion::BelowRejectThreshold;

    // For this test, the contract and the sender are on the same shard, even
    // the same account.
    let contract_id: AccountId = CONTRACT_ID.parse().unwrap();
    let sender_id = contract_id.clone();
    let dummy_receiver: AccountId = "a_dummy_receiver".parse().unwrap();
    let env = setup_test_runtime("test0".parse().unwrap(), protocol_version);

    let (
        remote_tx_included_without_congestion,
        local_tx_included_without_congestion,
        remote_tx_included_with_congestion,
        local_tx_included_with_congestion,
    ) = measure_tx_limit(env, sender_id, contract_id, dummy_receiver, upper_limit_congestion);

    assert_ne!(local_tx_included_without_congestion, 0);
    assert_ne!(remote_tx_included_with_congestion, 0);
    // local transactions should be limited
    assert!(
        local_tx_included_with_congestion < local_tx_included_without_congestion,
        "{local_tx_included_with_congestion} < {local_tx_included_without_congestion} failed"
    );
    // remote transactions in this case also start on the congested shard, so
    // they should be limited, too
    assert!(
        remote_tx_included_with_congestion < remote_tx_included_without_congestion,
        "{remote_tx_included_with_congestion} < {remote_tx_included_without_congestion} failed"
    );
}

/// Test that clients adjust included transactions based on the congestion level
/// of the local delayed receipts queue only, in this test with remote traffic.
///
/// We expect to see less transactions accepted on the congested shard, to give
/// more capacity towards processing delayed receipts. But in this test we stay
/// below `reject_tx_congestion_threshold`, meaning that the shard sending the
/// remote traffic to the congested shard should not stop, nor should it tighten
/// the limit on how many are accepted.
///
/// [`test_transaction_limit_for_local_congestion`] is a similar test but uses
/// only local receipts. [`test_transaction_filtering`] is even closer to this
/// test but goes beyond `reject_tx_congestion_threshold` to test the tx
/// rejection.
#[test]
fn test_transaction_limit_for_remote_congestion() {
    init_test_logger();
    if !ProtocolFeature::CongestionControl.enabled(PROTOCOL_VERSION) {
        return;
    }
    // We don't want to go into the TX rejection limit in this test.
    let upper_limit_congestion = UpperLimitCongestion::BelowRejectThreshold;

    let (
        remote_tx_included_without_congestion,
        local_tx_included_without_congestion,
        remote_tx_included_with_congestion,
        local_tx_included_with_congestion,
    ) = measure_remote_tx_limit(upper_limit_congestion);

    assert_ne!(remote_tx_included_without_congestion, 0);
    assert_ne!(remote_tx_included_with_congestion, 0);
    assert_ne!(local_tx_included_without_congestion, 0);
    assert_ne!(local_tx_included_with_congestion, 0);

    // local transactions should be limited
    assert!(
        local_tx_included_with_congestion < local_tx_included_without_congestion,
        "{local_tx_included_with_congestion} < {local_tx_included_without_congestion} failed"
    );
    // remote transactions should be unaffected
    assert!(
        remote_tx_included_with_congestion == remote_tx_included_without_congestion,
        "{remote_tx_included_with_congestion} == {remote_tx_included_without_congestion} failed"
    );
}

/// Test that clients stop including transactions to fully congested receivers.
#[test]
fn slow_test_transaction_filtering() {
    init_test_logger();

    if !ProtocolFeature::CongestionControl.enabled(PROTOCOL_VERSION) {
        return;
    }
    // This test should go beyond into the TX rejection limit in this test.
    let upper_limit_congestion = UpperLimitCongestion::AboveRejectThreshold;

    let (
        remote_tx_included_without_congestion,
        local_tx_included_without_congestion,
        remote_tx_included_with_congestion,
        local_tx_included_with_congestion,
    ) = measure_remote_tx_limit(upper_limit_congestion);

    assert_ne!(remote_tx_included_without_congestion, 0);
    assert_ne!(local_tx_included_without_congestion, 0);
    assert_ne!(local_tx_included_with_congestion, 0);

    // local transactions should be limited
    assert!(
        local_tx_included_with_congestion < local_tx_included_without_congestion,
        "{local_tx_included_with_congestion} < {local_tx_included_without_congestion} failed"
    );
    // remote transactions, with congestion, should be 0
    assert_eq!(remote_tx_included_with_congestion, 0);
}

enum UpperLimitCongestion {
    BelowRejectThreshold,
    AboveRejectThreshold,
}

/// Calls [`measure_tx_limit`] with accounts on three different shards.
fn measure_remote_tx_limit(
    upper_limit_congestion: UpperLimitCongestion,
) -> (usize, usize, usize, usize) {
    let contract_id: AccountId = CONTRACT_ID.parse().unwrap();
    let remote_id: AccountId = "test1.near".parse().unwrap();
    let dummy_id: AccountId = "test2.near".parse().unwrap();
    let env = setup_test_runtime(remote_id.clone(), PROTOCOL_VERSION);

    let tip = env.clients[0].chain.head().unwrap();
    let shard_layout = env.clients[0].epoch_manager.get_shard_layout(&tip.epoch_id).unwrap();
    let contract_shard_id = shard_layout.account_id_to_shard_id(&contract_id);
    let remote_shard_id = shard_layout.account_id_to_shard_id(&remote_id);
    let dummy_shard_id = shard_layout.account_id_to_shard_id(&dummy_id);

    // For a clean test setup, ensure we use 3 different shards.
    assert_ne!(remote_shard_id, contract_shard_id);
    assert_ne!(dummy_shard_id, contract_shard_id);
    assert_ne!(dummy_shard_id, remote_shard_id);

    measure_tx_limit(env, remote_id, contract_id, dummy_id, upper_limit_congestion)
}

/// Create the target incoming congestion level and measure the difference of
/// included transactions with and without the congestion, on the local shard
/// and on remote shards sending to the congested shard.
///
/// This helper function operates on three accounts. One account has the
/// contract that will be congested. Another account is used to send remote
/// traffic to that shard. And lastly, we send dummy transactions from the
/// congested account to a third account, to observe the limit applied to it.
///
/// The caller can choose to place the accounts on different shards or on the
/// same shard.
fn measure_tx_limit(
    mut env: TestEnv,
    remote_id: AccountId,
    contract_id: AccountId,
    dummy_receiver: AccountId,
    upper_limit_congestion: UpperLimitCongestion,
) -> (usize, usize, usize, usize) {
    let mut nonce = 1;
    setup_contract(&mut env, &mut nonce);
    if remote_id != contract_id {
        setup_account(&mut env, &mut nonce, &remote_id, &ACCOUNT_PARENT_ID.parse().unwrap());
    }

    let remote_signer = InMemorySigner::test_signer(&remote_id);
    let local_signer = InMemorySigner::test_signer(&contract_id);
    let tip = env.clients[0].chain.head().unwrap();
    let shard_layout = env.clients[0].epoch_manager.get_shard_layout(&tip.epoch_id).unwrap();
    let remote_shard_id = shard_layout.account_id_to_shard_id(&remote_id);
    let contract_shard_id = shard_layout.account_id_to_shard_id(&contract_id);

    // put in enough transactions to create up to
    // `reject_tx_congestion_threshold` incoming congestion
    let config = head_congestion_control_config(&env);
    let upper_limit_congestion = match upper_limit_congestion {
        UpperLimitCongestion::BelowRejectThreshold => config.reject_tx_congestion_threshold,
        UpperLimitCongestion::AboveRejectThreshold => config.reject_tx_congestion_threshold * 2.0,
    };

    let num_full_congestion = config.max_congestion_incoming_gas / (100 * 10u64.pow(12));
    let n = num_full_congestion as f64 * upper_limit_congestion;
    // Key of new account starts at block_height * 1_000_000
    let tip = env.clients[0].chain.head().unwrap();
    let mut nonce = tip.height * 1_000_000 + 1;
    submit_n_100tgas_fns(&mut env, n as u32, &mut nonce, &remote_signer);
    let tip = env.clients[0].chain.head().unwrap();
    // submit enough cheap transaction to at least fill the tx limit once
    submit_n_cheap_fns(&mut env, 1000, &mut nonce, &local_signer, &dummy_receiver);
    env.produce_block(0, tip.height + 1);

    // Produce blocks until all transactions are included.
    let timeout = 1000;
    let mut all_included = false;
    let mut remote_tx_included_without_congestion = 0;
    let mut local_tx_included_without_congestion = 0;
    for i in 2..timeout {
        let height = tip.height + i;
        env.produce_block(0, height);

        let remote_chunk = head_chunk(&env, remote_shard_id);
        let contract_chunk = head_chunk(&env, contract_shard_id);
        let remote_num_tx = remote_chunk.to_transactions().len();
        let local_num_tx = contract_chunk.to_transactions().len();

        if i == 2 {
            remote_tx_included_without_congestion = remote_num_tx;
            local_tx_included_without_congestion = local_num_tx;
        }
        if remote_num_tx == 0 && local_num_tx == 0 {
            all_included = true;
            break;
        }
    }
    assert!(all_included, "loop timed out before all transactions were included");

    // Now we expect the contract's shard to have non-trivial incoming
    // congestion.
    let congestion_info = head_congestion_info(&env, contract_shard_id);
    let incoming_congestion = congestion_info.incoming_congestion(&config);
    let congestion_level = congestion_info.localized_congestion_level(&config);
    // congestion should be non-trivial and below the upper limit
    assert!(
        incoming_congestion > upper_limit_congestion / 2.0,
        "{incoming_congestion} > {upper_limit_congestion} / 2 failed, {congestion_info:?}"
    );
    assert!(
        congestion_level < upper_limit_congestion,
        "{congestion_level} < {upper_limit_congestion} failed, {congestion_info:?}"
    );

    // Send some more transactions to see how many will be accepted now with congestion.
    submit_n_100tgas_fns(&mut env, n as u32, &mut nonce, &remote_signer);
    submit_n_cheap_fns(&mut env, 1000, &mut nonce, &local_signer, &dummy_receiver);
    let tip = env.clients[0].chain.head().unwrap();
    env.produce_block(0, tip.height + 1);
    env.produce_block(0, tip.height + 2);
    let remote_chunk = head_chunk(&env, remote_shard_id);
    let local_chunk = head_chunk(&env, contract_shard_id);
    let remote_tx_included_with_congestion = remote_chunk.to_transactions().len();
    let local_tx_included_with_congestion = local_chunk.to_transactions().len();
    (
        remote_tx_included_without_congestion,
        local_tx_included_without_congestion,
        remote_tx_included_with_congestion,
        local_tx_included_with_congestion,
    )
}

/// Test that RPC clients stop accepting transactions when the receiver is
/// congested.
#[test]
fn test_rpc_client_rejection() {
    let sender_id: AccountId = "test0".parse().unwrap();
    let mut env = setup_test_runtime(sender_id.clone(), PROTOCOL_VERSION);

    // prepare a contract to call
    let mut nonce = 10;
    setup_contract(&mut env, &mut nonce);

    let signer = InMemorySigner::test_signer(&sender_id);

    // Check we can send transactions at the start.
    let fn_tx = new_fn_call_100tgas(
        &mut nonce,
        &signer,
        *env.clients[0].chain.head_header().unwrap().hash(),
    );
    let response = env.tx_request_handlers[0].process_tx(fn_tx, false, false);
    assert_eq!(response, ProcessTxResponse::ValidTx);

    // Congest the network with a burst of 100 PGas.
    submit_n_100tgas_fns(&mut env, 1_000, &mut nonce, &signer);

    // Allow transactions to enter the chain and enough receipts to arrive at
    // the receiver shard for it to become congested.
    let tip = env.clients[0].chain.head().unwrap();
    for i in 1..10 {
        env.produce_block(0, tip.height + i);
    }

    // Check that congestion control rejects new transactions.
    let fn_tx = new_fn_call_100tgas(
        &mut nonce,
        &signer,
        *env.clients[0].chain.head_header().unwrap().hash(),
    );
    let response = env.tx_request_handlers[0].process_tx(fn_tx, false, false);

    if ProtocolFeature::CongestionControl.enabled(PROTOCOL_VERSION) {
        assert_matches!(
            response,
            ProcessTxResponse::InvalidTx(InvalidTxError::ShardCongested { .. })
        );
    } else {
        assert_eq!(response, ProcessTxResponse::ValidTx);
    }
}
