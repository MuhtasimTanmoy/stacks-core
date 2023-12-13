// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2023 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

/// This file implements the interface to the StackerDB smart contract for loading the DB's config.
/// The smart contract must conform to this trait:
///
/// ;; Any StackerDB smart contract must conform to this trait.
/// (define-trait stackerdb-trait
///
///     ;; Get the list of (signer, num-slots) that make up this DB
///     (define-public (stackerdb-get-signer-slots) (response (list 4096 { signer: principal, num-slots: uint }) uint))
///
///     ;; Get the control metadata for this DB
///     (define-public (stackerdb-get-config)
///         (response {
///             chunk-size: uint,
///             write-freq: uint,
///             max-writes: uint,
///             max-neighbors: uint,
///             hint-replicas: (list 128 { addr: (list 16 uint), port: uint, public-key-hash: (buff 20) })
///         },
///         uint))
/// )
use std::collections::{HashMap, HashSet};
use std::mem;

use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::clarity::ClarityConnection;
use clarity::vm::database::BurnStateDB;
use clarity::vm::types::{
    BufferLength, FixedFunction, FunctionType, ListTypeData, PrincipalData,
    QualifiedContractIdentifier, SequenceSubtype, StandardPrincipalData, TupleTypeSignature,
    TypeSignature,
};
use clarity::vm::ClarityName;
use lazy_static::lazy_static;
use stacks_common::types::chainstate::{StacksAddress, StacksBlockId};
use stacks_common::types::net::PeerAddress;
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::Hash160;

use crate::chainstate::burn::db::sortdb::SortitionDB;
use crate::chainstate::burn::db::v2::SortitionDb;
use crate::chainstate::nakamoto::NakamotoChainState;
use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::Error as chainstate_error;
use crate::chainstate::stacks::db::v2::stacks_chainstate_db::ChainStateDb;
use crate::chainstate::stacks::index::db::DbConnection;
use crate::chainstate::stacks::index::trie_db::TrieDb;
use crate::clarity_vm::clarity::{ClarityReadOnlyConnection, Error as clarity_error};
use crate::net::stackerdb::{
    StackerDBConfig, StackerDBs, STACKERDB_INV_MAX, STACKERDB_MAX_CHUNK_SIZE,
};
use crate::net::{Error as NetError, NeighborAddress};

const MAX_HINT_REPLICAS: u32 = 128;

lazy_static! {
    pub static ref REQUIRED_FUNCTIONS: [(ClarityName, TypeSignature); 2] = [
        (
            "stackerdb-get-signer-slots".into(),
            TypeSignature::new_response(
                ListTypeData::new_list(
                    TupleTypeSignature::try_from(vec![
                        ("signer".into(), TypeSignature::PrincipalType),
                        ("num-slots".into(), TypeSignature::UIntType)
                    ])
                    .expect("FATAL: failed to construct signer list type")
                    .into(),
                    STACKERDB_INV_MAX
                )
                .expect("FATAL: could not construct signer list type")
                .into(),
                TypeSignature::UIntType
            ).expect("FATAL: failed to construct response with signer slots"),
        ),
        (
            "stackerdb-get-config".into(),
            TypeSignature::new_response(
                TypeSignature::TupleType(
                    TupleTypeSignature::try_from(vec![
                        ("chunk-size".into(), TypeSignature::UIntType),
                        ("write-freq".into(), TypeSignature::UIntType),
                        ("max-writes".into(), TypeSignature::UIntType),
                        ("max-neighbors".into(), TypeSignature::UIntType),
                        ("hint-replicas".into(), ListTypeData::new_list(
                            TypeSignature::TupleType(
                                TupleTypeSignature::try_from(vec![
                                    ("addr".into(), ListTypeData::new_list(TypeSignature::UIntType, 16)
                                        .expect("FATAL: invalid IP address list")
                                        .into()),
                                    ("port".into(), TypeSignature::UIntType),
                                    ("public-key-hash".into(),
                                        // can't use BUFF_20 here because it's also in a
                                        // lazy_static! block
                                        TypeSignature::SequenceType(SequenceSubtype::BufferType(BufferLength::try_from(20u32).expect("FATAL: could not create (buff 20)"))))
                                ])
                                .expect("FATAL: unable to construct hint-replicas type")
                                .into()),
                            MAX_HINT_REPLICAS)
                            .expect("FATAL: failed to construct hint-replicas list type")
                            .into())
                    ]).expect("FATAL: unable to construct config type")).into(),
                TypeSignature::UIntType
            ).expect("FATAL: unable to construct config response type")
        )
    ];
}

impl StackerDBConfig
{
    /// Check that a smart contract is consistent with being a StackerDB controller.
    /// Returns Ok(..) if the contract is valid
    /// Returns Err(reason) if the contract is invalid.  A human-readable reason will be given.
    fn is_contract_valid(epoch: &StacksEpochId, analysis: ContractAnalysis) -> Result<(), String> {
        for (name, func_return_type) in REQUIRED_FUNCTIONS.iter() {
            let func = if let Some(f) = analysis.read_only_function_types.get(name) {
                f
            } else if let Some(f) = analysis.public_function_types.get(name) {
                f
            } else {
                let reason = format!("Contract is missing function '{}'", name);
                return Err(reason);
            };

            match func {
                FunctionType::Fixed(FixedFunction { args, returns }) => {
                    if args.len() != 0 {
                        let reason = format!("Contract function '{}' has an invalid signature: it must take zero arguments", name);
                        return Err(reason);
                    }
                    if !func_return_type
                        .admits_type(epoch, &returns)
                        .unwrap_or(false)
                    {
                        let reason = format!("Contract function '{}' has an invalid return type: expected {:?}, got {:?}", name, func_return_type, returns);
                        return Err(reason);
                    }
                }
                _ => {
                    let reason = format!("Contract function '{}' is not a fixed function", name);
                    return Err(reason);
                }
            }
        }
        Ok(())
    }

    /// Evaluate the contract to get its signer slots
    fn eval_signer_slots<ChainDB>(
        chainstate: &mut StacksChainState<ChainDB>,
        burn_dbconn: &dyn BurnStateDB,
        contract_id: &QualifiedContractIdentifier,
        tip: &StacksBlockId,
    ) -> Result<Vec<(StacksAddress, u32)>, NetError> 
    where
        ChainDB: ChainStateDb
    {
        let value = chainstate.eval_read_only(
            burn_dbconn,
            tip,
            contract_id,
            "(stackerdb-get-signer-slots)",
        )?;

        let result = value.expect_result();
        let slot_list = match result {
            Err(err_val) => {
                let err_code = err_val.expect_u128();
                let reason = format!(
                    "Contract {} failed to run `stackerdb-get-signer-slots`: error u{}",
                    contract_id, &err_code
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }
            Ok(ok_val) => ok_val.expect_list(),
        };

        let mut total_num_slots = 0u32;
        let mut ret = vec![];
        for slot_value in slot_list.into_iter() {
            let slot_data = slot_value.expect_tuple();
            let signer_principal = slot_data
                .get("signer")
                .expect("FATAL: no 'signer'")
                .clone()
                .expect_principal();
            let num_slots_uint = slot_data
                .get("num-slots")
                .expect("FATAL: no 'num-slots'")
                .clone()
                .expect_u128();

            if num_slots_uint > (STACKERDB_INV_MAX as u128) {
                let reason = format!(
                    "Contract {} stipulated more than maximum number of slots for one signer ({})",
                    contract_id, STACKERDB_INV_MAX
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }
            let num_slots = num_slots_uint as u32;
            total_num_slots =
                total_num_slots
                    .checked_add(num_slots)
                    .ok_or(NetError::OverflowError(format!(
                        "Contract {} stipulates more than u32::MAX slots",
                        &contract_id
                    )))?;

            if total_num_slots > STACKERDB_INV_MAX.into() {
                let reason = format!(
                    "Contract {} stipulated more than the maximum number of slots",
                    contract_id
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }

            // standard principals only
            let addr = match signer_principal {
                PrincipalData::Contract(..) => {
                    let reason = format!("Contract {} stipulated a contract principal as a writer, which is not supported", contract_id);
                    warn!("{}", &reason);
                    return Err(NetError::InvalidStackerDBContract(
                        contract_id.clone(),
                        reason,
                    ));
                }
                PrincipalData::Standard(StandardPrincipalData(version, bytes)) => StacksAddress {
                    version,
                    bytes: Hash160(bytes),
                },
            };

            ret.push((addr, num_slots));
        }
        Ok(ret)
    }

    /// Evaluate the contract to get its config
    fn eval_config<ChainDB>(
        chainstate: &mut StacksChainState<ChainDB>,
        burn_dbconn: &dyn BurnStateDB,
        contract_id: &QualifiedContractIdentifier,
        tip: &StacksBlockId,
        signers: Vec<(StacksAddress, u32)>,
    ) -> Result<StackerDBConfig, NetError> 
    where
        ChainDB: ChainStateDb
    {
        let value =
            chainstate.eval_read_only(burn_dbconn, tip, contract_id, "(stackerdb-get-config)")?;

        let result = value.expect_result();
        let config_tuple = match result {
            Err(err_val) => {
                let err_code = err_val.expect_u128();
                let reason = format!(
                    "Contract {} failed to run `stackerdb-get-config`: err u{}",
                    contract_id, &err_code
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }
            Ok(ok_val) => ok_val.expect_tuple(),
        };

        let chunk_size = config_tuple
            .get("chunk-size")
            .expect("FATAL: missing 'chunk-size'")
            .clone()
            .expect_u128();

        if chunk_size > STACKERDB_MAX_CHUNK_SIZE as u128 {
            let reason = format!(
                "Contract {} stipulates a chunk size beyond STACKERDB_MAX_CHUNK_SIZE",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let write_freq = config_tuple
            .get("write-freq")
            .expect("FATAL: missing 'write-freq'")
            .clone()
            .expect_u128();
        if write_freq > u64::MAX as u128 {
            let reason = format!(
                "Contract {} stipulates a write frequency beyond u64::MAX",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let max_writes = config_tuple
            .get("max-writes")
            .expect("FATAL: missing 'max-writes'")
            .clone()
            .expect_u128();
        if max_writes > u32::MAX as u128 {
            let reason = format!(
                "Contract {} stipulates a max-write bound beyond u32::MAX",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let max_neighbors = config_tuple
            .get("max-neighbors")
            .expect("FATAL: missing 'max-neighbors'")
            .clone()
            .expect_u128();
        if max_neighbors > usize::MAX as u128 {
            let reason = format!(
                "Contract {} stipulates a maximum number of neighbors beyond usize::MAX",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let hint_replicas_list = config_tuple
            .get("hint-replicas")
            .expect("FATAL: missing 'hint-replicas'")
            .clone()
            .expect_list();
        let mut hint_replicas = vec![];
        for hint_replica_value in hint_replicas_list.into_iter() {
            let hint_replica_data = hint_replica_value.expect_tuple();

            let addr_byte_list = hint_replica_data
                .get("addr")
                .expect("FATAL: missing 'addr'")
                .clone()
                .expect_list();
            let port = hint_replica_data
                .get("port")
                .expect("FATAL: missing 'port'")
                .clone()
                .expect_u128();
            let pubkey_hash_bytes = hint_replica_data
                .get("public-key-hash")
                .expect("FATAL: missing 'public-key-hash")
                .clone()
                .expect_buff_padded(20, 0);

            let mut addr_bytes = vec![];
            for byte_val in addr_byte_list.into_iter() {
                let byte = byte_val.expect_u128();
                if byte > (u8::MAX as u128) {
                    let reason = format!(
                        "Contract {} stipulates an addr byte above u8::MAX",
                        contract_id
                    );
                    warn!("{}", &reason);
                    return Err(NetError::InvalidStackerDBContract(
                        contract_id.clone(),
                        reason,
                    ));
                }
                addr_bytes.push(byte as u8);
            }
            if addr_bytes.len() != 16 {
                let reason = format!(
                    "Contract {} did not stipulate a full 16-octet IP address",
                    contract_id
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }

            if port < 1024 || port > ((u16::MAX - 1) as u128) {
                let reason = format!(
                    "Contract {} stipulates a port lower than 1024 or above u16::MAX - 1",
                    contract_id
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }

            let mut pubkey_hash_slice = [0u8; 20];
            pubkey_hash_slice.copy_from_slice(&pubkey_hash_bytes[0..20]);

            let peer_addr = PeerAddress::from_slice(&addr_bytes).expect("FATAL: not 16 bytes");
            let naddr = NeighborAddress {
                addrbytes: peer_addr,
                port: port as u16,
                public_key_hash: Hash160(pubkey_hash_slice),
            };
            hint_replicas.push(naddr);
        }

        Ok(StackerDBConfig {
            chunk_size: chunk_size as u64,
            signers,
            write_freq: write_freq as u64,
            max_writes: max_writes as u32,
            hint_replicas,
            max_neighbors: max_neighbors as usize,
        })
    }

    /// Load up the DB config from the controlling smart contract as of the current Stacks chain
    /// tip
    pub fn from_smart_contract<SortDB, ChainDB>(
        chainstate: &mut StacksChainState<ChainDB>,
        sortition_db: &SortDB,
        contract_id: &QualifiedContractIdentifier,
    ) -> Result<StackerDBConfig, NetError> 
    where
        SortDB: SortitionDb,
        ChainDB: ChainStateDb
    {
        let chain_tip =
            NakamotoChainState::get_canonical_block_header(chainstate.db(), sortition_db)?
                .ok_or(NetError::NoSuchStackerDB(contract_id.clone()))?;

        let burn_tip = SortitionDB::get_block_snapshot_consensus(
            sortition_db.conn(),
            &chain_tip.consensus_hash,
        )?
        .expect("FATAL: missing snapshot for Stacks block");

        let chain_tip_hash = StacksBlockId::new(
            &chain_tip.consensus_hash,
            &chain_tip.anchored_header.block_hash(),
        );
        let cur_epoch = SortitionDB::get_stacks_epoch(sortition_db.conn(), burn_tip.block_height)?
            .expect("FATAL: no epoch defined");

        let dbconn = sortition_db.index_conn();

        // check the target contract
        let res =
            chainstate.maybe_read_only_clarity_tx(&dbconn, &chain_tip_hash, |clarity_tx| {
                // determine if this contract exists and conforms to this trait
                clarity_tx.with_clarity_db_readonly(|db| {
                    // contract must exist or this errors out
                    let analysis = db
                        .load_contract_analysis(contract_id)
                        .ok_or(NetError::NoSuchStackerDB(contract_id.clone()))?;

                    // contract must be consistent with StackerDB control interface
                    if let Err(invalid_reason) =
                        Self::is_contract_valid(&cur_epoch.epoch_id, analysis)
                    {
                        let reason = format!(
                            "Contract {} does not conform to StackerDB trait: {}",
                            contract_id, invalid_reason
                        );
                        warn!("{}", &reason);
                        return Err(NetError::InvalidStackerDBContract(
                            contract_id.clone(),
                            reason,
                        ));
                    }

                    Ok(())
                })
            })?;

        if res.is_none() {
            let reason = format!(
                "Could not evaluate contract {} at {}",
                contract_id, &chain_tip_hash
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        } else if let Some(Err(e)) = res {
            warn!(
                "Could not use contract {} for StackerDB: {:?}",
                contract_id, &e
            );
            return Err(e);
        }

        // evaluate the contract for these two functions
        let signers = Self::eval_signer_slots(chainstate, &dbconn, contract_id, &chain_tip_hash)?;
        let config = Self::eval_config(chainstate, &dbconn, contract_id, &chain_tip_hash, signers)?;
        Ok(config)
    }
}
