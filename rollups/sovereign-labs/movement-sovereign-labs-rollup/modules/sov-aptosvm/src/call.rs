use anyhow::Result;
use aptos_sdk::rest_client::aptos;
use revm::primitives::CfgEnv;
use sov_modules_api::CallResponse;
use sov_state::WorkingSet;
use serde_json;
use aptos_crypto::hash::CryptoHash;
use chrono::{Utc};
use aptos_consensus_types::block::Block;

use crate::AptosVm;

use aptos_crypto::{HashValue, ValidCryptoMaterialStringExt};
use aptos_crypto::ed25519::Ed25519PublicKey;
use aptos_db::AptosDB;
use aptos_executor::block_executor::BlockExecutor;
use aptos_executor::db_bootstrapper::{generate_waypoint, maybe_bootstrap};
use aptos_executor_types::BlockExecutorTrait;
use aptos_sdk::rest_client::aptos_api_types::MAX_RECURSIVE_TYPES_ALLOWED;
use aptos_sdk::transaction_builder::TransactionFactory;
use aptos_sdk::types::{AccountKey, LocalAccount};
use aptos_state_view::account_with_state_view::AsAccountWithStateView;
use aptos_storage_interface::DbReaderWriter;
use aptos_storage_interface::state_view::DbStateViewAtVersion;
use aptos_types::account_address::AccountAddress;
use aptos_types::account_config::aptos_test_root_address;
use aptos_types::account_view::AccountView;
use aptos_types::block_info::BlockInfo;
use aptos_types::block_metadata::BlockMetadata;
use aptos_types::chain_id::ChainId;
use aptos_types::ledger_info::{generate_ledger_info_with_sig, LedgerInfo};
use aptos_types::mempool_status::{MempoolStatus, MempoolStatusCode};
use aptos_types::validator_signer::ValidatorSigner;
use aptos_vm::AptosVM;
use aptos_vm_genesis::{GENESIS_KEYPAIR, test_genesis_change_set_and_validators};
use aptos_types::transaction::{Transaction};
use aptos_types::trusted_state::{TrustedState, TrustedStateChange};

use borsh::{BorshDeserialize, BorshSerialize};
use sov_movevm_types::aptos::transaction::{TransactionWrapper};



#[cfg_attr(
    feature = "native",
    derive(serde::Serialize),
    derive(serde::Deserialize)
)]
#[derive(borsh::BorshDeserialize, borsh::BorshSerialize, Debug, PartialEq, Clone)]
pub struct CallMessage {
    pub serialized_txs: Vec<Vec<u8>>,
    #[cfg(feature = "naked")]
    pub tx : TransactionWrapper,
}

impl<C: sov_modules_api::Context> AptosVm<C> {

    #[cfg(feature = "aptos-consensus")]
    pub(crate) fn execute_call_with_aptos_consensus(
        &self,
        serialized_txs: Vec<Vec<u8>>,
        _context: &C,
        working_set: &mut WorkingSet<C::Storage>,
    ) -> Result<CallResponse> {

        // timestamp
        let unix_now = Utc::now().timestamp() as u64;

        // get db for reference
        let db = self.get_db(working_set)?;

        // get the validator signer
        let validator_signer = self.get_validator_signer(working_set)?;

        // get the parent (genesis block)
        let parent_block_id = self.get_genesis_hash(working_set)?;

        // produce the block meta
        let latest_ledger_info = db.reader.get_latest_ledger_info()?;
        let next_epoch = latest_ledger_info.ledger_info().next_block_epoch();
        let block_id = HashValue::random();
        let block_meta = Transaction::BlockMetadata(BlockMetadata::new(
            block_id,
            next_epoch,
            0,
            validator_signer.author(),
            vec![],
            vec![],
            unix_now,
        ));

        let mut txs = vec![];
        for serialized_tx in serialized_txs {
            let tx = serde_json::from_slice::<Transaction>(&serialized_tx)
            .expect("Failed to deserialize transaction");
            txs.push(tx.clone());
            let hash = tx.hash(); // diem crypto hasher
            let str_hash = hash.to_string();
            self.transactions.set(&str_hash, &serialized_tx, working_set);
        }

        // store the checkpoint
        let checkpoint = Transaction::StateCheckpoint(HashValue::random());

        // form the complete block
        let mut block = vec![

        ];
        block.push(block_meta);
        block.extend(txs);
        block.push(checkpoint);

        drop(db); // drop the db from above so that the executor can use RocksDB

        // execute the transaction in Aptos
        let executor = self.get_executor(working_set)?;
        // let parent_block_id = executor.committed_block_id();

        println!("EXECUTING BLOCK {:?} {:?}", block_id, parent_block_id);
        let result = executor
            .execute_block((block_id, block).into(), parent_block_id, None)?;

        // sign for the the ledger
        let ledger_info = LedgerInfo::new(
            BlockInfo::new(
                next_epoch,
                0,
                block_id,
                result.root_hash(),
                result.version(),
                unix_now,
                result.epoch_state().clone(),
            ),
            HashValue::zero(),
        );

        println!("COMMITTING BLOCK: {:?} {:?}", block_id, parent_block_id);
        let li = generate_ledger_info_with_sig(&[validator_signer], ledger_info);
        executor.commit_blocks(vec![block_id], li.clone())
        .expect("Failed to commit blocks");

        // manage epoch an parent block id
        if li.ledger_info().ends_epoch() {
            let epoch_genesis_id = Block::make_genesis_block_from_ledger_info(li.ledger_info()).id();
            self.genesis_hash.set(&epoch_genesis_id.to_vec(), working_set);
        }

        drop(executor);
        // prove state 
        let db_too = self.get_db(working_set)?;
        let state_proof = db_too.reader.get_state_proof(self.get_known_version(working_set)?)?;
        let trusted_state = TrustedState::from_epoch_waypoint(self.get_waypoint(working_set)?);
        let trusted_state = match trusted_state.verify_and_ratchet(&state_proof) {
            Ok(TrustedStateChange::Epoch { new_state, .. }) => new_state,
            _ => panic!("unexpected state change"),
        };
        self.waypoint.set(
            &trusted_state.waypoint().to_string(),
            working_set
        );
        self.known_version.set(
            &trusted_state.version(),
            working_set
        );

        // TODO: may want to use a lower level of execution abstraction
        // TODO: see https://github.com/movemntdev/aptos-core/blob/main/aptos-move/block-executor/src/executor.rs#L73
        // TODO: for an entrypoint that does not require a block.
        Ok(CallResponse::default())

    }

    pub(crate) fn execute_call_with_naked_vm(
        &self,
        serialized_txs: Vec<Vec<u8>>,
        _context: &C,
        working_set: &mut WorkingSet<C::Storage>,
    ) -> Result<CallResponse> {

        let vm = self.get_aptos_vm(working_set)?;
        Ok(CallResponse::default())

    }

    pub(crate) fn execute_call(
        &self,
        serialized_txs: Vec<Vec<u8>>,
        _context: &C,
        working_set: &mut WorkingSet<C::Storage>,
    ) -> Result<CallResponse> {

       #[cfg(feature = "aptos-consensus")]
       {
            self.execute_call_with_aptos_consensus(serialized_txs, _context, working_set)
       }

       #[cfg(not(feature = "aptos-consensus"))]
       {
            self.execute_call_with_naked_vm(serialized_txs, _context, working_set)
       }

    }
}
