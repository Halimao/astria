//! The tests in this file are snapshot tests that are expected to break
//! if breaking changes are made to the application, in particular,
//! any changes that can affect the state tree and thus the app hash.
//!
//! If these tests break due to snapshot mismatches, you can update the snapshots
//! with `cargo insta review`, but you MUST mark the respective PR as breaking.
//!
//! Note: there are two actions not tested here: `Ics20Withdrawal` and `IbcRelay`.
//! These are due to the extensive setup needed to test them.
//! If changes are made to the execution results of these actions, manual testing is required.

use std::{
    collections::HashMap,
    sync::Arc,
};

use astria_core::{
    primitive::v1::RollupId,
    protocol::transaction::v1alpha1::{
        action::{
            BridgeLockAction,
            BridgeSudoChangeAction,
            BridgeUnlockAction,
            IbcRelayerChangeAction,
            SequenceAction,
            TransferAction,
        },
        Action,
        TransactionParams,
        UnsignedTransaction,
    },
    sequencer::{
        Account,
        AddressPrefixes,
        UncheckedGenesisState,
    },
    sequencerblock::v1alpha1::block::Deposit,
};
use cnidarium::StateDelta;
use penumbra_ibc::params::IBCParameters;
use prost::Message as _;
use tendermint::{
    abci,
    abci::types::CommitInfo,
    block::Round,
    Hash,
    Time,
};

use crate::{
    app::test_utils::{
        address_from_hex_string,
        default_fees,
        default_genesis_accounts,
        get_alice_signing_key_and_address,
        get_bridge_signing_key_and_address,
        initialize_app,
        initialize_app_with_storage,
        BOB_ADDRESS,
        CAROL_ADDRESS,
    },
    asset::get_native_asset,
    bridge::state_ext::StateWriteExt as _,
    proposal::commitment::generate_rollup_datas_commitment,
};

/// XXX: This should be expressed in terms of `crate::app::test_utils::unchecked_genesis_state` to
/// be consistent everywhere. `get_alice_signing_key` already is, why not this?
fn unchecked_genesis_state() -> UncheckedGenesisState {
    let (_, alice_address) = get_alice_signing_key_and_address();
    UncheckedGenesisState {
        accounts: vec![],
        address_prefixes: AddressPrefixes {
            base: crate::address::get_base_prefix().to_string(),
        },
        authority_sudo_address: alice_address,
        ibc_sudo_address: alice_address,
        ibc_relayer_addresses: vec![],
        native_asset_base_denomination: "nria".to_string(),
        ibc_params: IBCParameters::default(),
        allowed_fee_assets: vec!["nria".parse().unwrap()],
        fees: default_fees(),
    }
}

#[tokio::test]
async fn app_genesis_snapshot() {
    let app = initialize_app(None, vec![]).await;
    insta::assert_json_snapshot!(app.app_hash.as_bytes());
}

#[tokio::test]
async fn app_finalize_block_snapshot() {
    let (alice_signing_key, _) = get_alice_signing_key_and_address();
    let (mut app, storage) = initialize_app_with_storage(None, vec![]).await;

    let bridge_address = crate::address::base_prefixed([99; 20]);
    let rollup_id = RollupId::from_unhashed_bytes(b"testchainid");
    let asset = get_native_asset().clone();

    let mut state_tx = StateDelta::new(app.state.clone());
    state_tx.put_bridge_account_rollup_id(&bridge_address, &rollup_id);
    state_tx
        .put_bridge_account_ibc_asset(&bridge_address, &asset)
        .unwrap();
    app.apply(state_tx);

    // the state changes must be committed, as `finalize_block` will execute the
    // changes on the latest snapshot, not the app's `StateDelta`.
    app.prepare_commit(storage.clone()).await.unwrap();
    app.commit(storage.clone()).await;

    let amount = 100;
    let lock_action = BridgeLockAction {
        to: bridge_address,
        amount,
        asset: asset.clone(),
        fee_asset: asset.clone(),
        destination_chain_address: "nootwashere".to_string(),
    };
    let sequence_action = SequenceAction {
        rollup_id,
        data: b"hello world".to_vec(),
        fee_asset: asset.clone(),
    };
    let tx = UnsignedTransaction {
        params: TransactionParams::builder()
            .nonce(0)
            .chain_id("test")
            .build(),
        actions: vec![lock_action.into(), sequence_action.into()],
    };

    let signed_tx = tx.into_signed(&alice_signing_key);

    let expected_deposit = Deposit::new(
        bridge_address,
        rollup_id,
        amount,
        asset,
        "nootwashere".to_string(),
    );
    let deposits = HashMap::from_iter(vec![(rollup_id, vec![expected_deposit.clone()])]);
    let commitments = generate_rollup_datas_commitment(&[signed_tx.clone()], deposits.clone());

    let timestamp = Time::unix_epoch();
    let block_hash = Hash::try_from([99u8; 32].to_vec()).unwrap();
    let finalize_block = abci::request::FinalizeBlock {
        hash: block_hash,
        height: 1u32.into(),
        time: timestamp,
        next_validators_hash: Hash::default(),
        proposer_address: [0u8; 20].to_vec().try_into().unwrap(),
        txs: commitments.into_transactions(vec![signed_tx.to_raw().encode_to_vec().into()]),
        decided_last_commit: CommitInfo {
            votes: vec![],
            round: Round::default(),
        },
        misbehavior: vec![],
    };

    app.finalize_block(finalize_block.clone(), storage.clone())
        .await
        .unwrap();
    app.commit(storage.clone()).await;
    insta::assert_json_snapshot!(app.app_hash.as_bytes());
}

// Note: this tests every action except for `Ics20Withdrawal` and `IbcRelay`.
//
// If new actions are added to the app, they must be added to this test,
// and the respective PR must be marked as breaking.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn app_execute_transaction_with_every_action_snapshot() {
    use astria_core::protocol::transaction::v1alpha1::action::{
        FeeAssetChangeAction,
        InitBridgeAccountAction,
        SudoAddressChangeAction,
    };

    let (alice_signing_key, _) = get_alice_signing_key_and_address();
    let (bridge_signing_key, bridge_address) = get_bridge_signing_key_and_address();
    let bob_address = address_from_hex_string(BOB_ADDRESS);
    let carol_address = address_from_hex_string(CAROL_ADDRESS);
    let mut accounts = default_genesis_accounts();
    accounts.push(Account {
        address: bridge_address,
        balance: 1_000_000_000,
    });

    let genesis_state = UncheckedGenesisState {
        accounts,
        ..unchecked_genesis_state()
    }
    .try_into()
    .unwrap();
    let (mut app, storage) = initialize_app_with_storage(Some(genesis_state), vec![]).await;

    // setup for ValidatorUpdate action
    let pub_key = tendermint::public_key::PublicKey::from_raw_ed25519(&[1u8; 32]).unwrap();
    let update = tendermint::validator::Update {
        pub_key,
        power: 100u32.into(),
    };

    let rollup_id = RollupId::from_unhashed_bytes(b"testchainid");
    let asset = get_native_asset().clone();

    let tx = UnsignedTransaction {
        params: TransactionParams::builder()
            .nonce(0)
            .chain_id("test")
            .build(),
        actions: vec![
            TransferAction {
                to: bob_address,
                amount: 333_333,
                asset: asset.clone(),
                fee_asset: asset.clone(),
            }
            .into(),
            SequenceAction {
                rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
                data: b"hello world".to_vec(),
                fee_asset: asset.clone(),
            }
            .into(),
            Action::ValidatorUpdate(update.clone()),
            IbcRelayerChangeAction::Addition(bob_address).into(),
            IbcRelayerChangeAction::Addition(carol_address).into(),
            IbcRelayerChangeAction::Removal(bob_address).into(),
            // TODO: should fee assets be stored in state?
            FeeAssetChangeAction::Addition("test-0".parse().unwrap()).into(),
            FeeAssetChangeAction::Addition("test-1".parse().unwrap()).into(),
            FeeAssetChangeAction::Removal("test-0".parse().unwrap()).into(),
            SudoAddressChangeAction {
                new_address: bob_address,
            }
            .into(),
        ],
    };

    let signed_tx = Arc::new(tx.into_signed(&alice_signing_key));
    app.execute_transaction(signed_tx).await.unwrap();

    let tx = UnsignedTransaction {
        params: TransactionParams::builder()
            .nonce(0)
            .chain_id("test")
            .build(),
        actions: vec![
            InitBridgeAccountAction {
                rollup_id,
                asset: asset.clone(),
                fee_asset: asset.clone(),
                sudo_address: None,
                withdrawer_address: None,
            }
            .into(),
        ],
    };
    let signed_tx = Arc::new(tx.into_signed(&bridge_signing_key));
    app.execute_transaction(signed_tx).await.unwrap();

    let tx = UnsignedTransaction {
        params: TransactionParams::builder()
            .chain_id("test")
            .nonce(1)
            .build(),
        actions: vec![
            BridgeLockAction {
                to: bridge_address,
                amount: 100,
                asset: asset.clone(),
                fee_asset: asset.clone(),
                destination_chain_address: "nootwashere".to_string(),
            }
            .into(),
            BridgeUnlockAction {
                to: bob_address,
                amount: 10,
                fee_asset: asset.clone(),
                memo: vec![0u8; 32],
                bridge_address: None,
            }
            .into(),
            BridgeSudoChangeAction {
                bridge_address,
                new_sudo_address: Some(bob_address),
                new_withdrawer_address: Some(bob_address),
                fee_asset: asset.clone(),
            }
            .into(),
        ],
    };

    let signed_tx = Arc::new(tx.into_signed(&bridge_signing_key));
    app.execute_transaction(signed_tx).await.unwrap();

    app.prepare_commit(storage.clone()).await.unwrap();
    app.commit(storage.clone()).await;

    insta::assert_json_snapshot!(app.app_hash.as_bytes());
}
