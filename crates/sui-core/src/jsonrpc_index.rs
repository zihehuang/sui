// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! IndexStore supports creation of various ancillary indexes of state in SuiDataStore.
//! The main user of this data is the explorer.

use std::cmp::{max, min};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bincode::Options;
use itertools::Itertools;
use move_core_types::language_storage::{ModuleId, StructTag, TypeTag};
use parking_lot::ArcMutexGuard;
use prometheus::{
    register_int_counter_vec_with_registry, register_int_counter_with_registry, IntCounter,
    IntCounterVec, Registry,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use typed_store::rocksdb::compaction_filter::Decision;
use typed_store::TypedStoreError;

use sui_json_rpc_types::{SuiObjectDataFilter, TransactionFilter};
use sui_storage::mutex_table::MutexTable;
use sui_storage::sharded_lru::ShardedLruCache;
use sui_types::base_types::{
    ObjectDigest, ObjectID, SequenceNumber, SuiAddress, TransactionDigest, TxSequenceNumber,
};
use sui_types::base_types::{ObjectInfo, ObjectRef};
use sui_types::digests::TransactionEventsDigest;
use sui_types::dynamic_field::{self, DynamicFieldInfo};
use sui_types::effects::TransactionEvents;
use sui_types::error::{SuiError, SuiResult, UserInputError};
use sui_types::inner_temporary_store::TxCoins;
use sui_types::object::{Object, Owner};
use sui_types::parse_sui_struct_tag;
use sui_types::storage::error::Error as StorageError;
use tracing::{debug, info, instrument, trace};
use typed_store::rocks::{
    default_db_options, read_size_from_env, DBBatch, DBMap, DBMapTableConfigMap, DBOptions,
    MetricConf,
};
use typed_store::traits::Map;
use typed_store::DBMapUtils;

type OwnedMutexGuard<T> = ArcMutexGuard<parking_lot::RawMutex, T>;

type OwnerIndexKey = (SuiAddress, ObjectID);
type DynamicFieldKey = (ObjectID, ObjectID);
type EventId = (TxSequenceNumber, usize);
type EventIndex = (TransactionEventsDigest, TransactionDigest, u64);
type AllBalance = HashMap<TypeTag, TotalBalance>;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct CoinIndexKey2 {
    pub owner: SuiAddress,
    pub coin_type: String,
    // the balance of the coin inverted `!coin.balance` in order to force sorting of coins to be
    // from greatest to least
    pub inverted_balance: u64,
    pub object_id: ObjectID,
}

impl CoinIndexKey2 {
    pub fn new_from_cursor(
        owner: SuiAddress,
        coin_type: String,
        inverted_balance: u64,
        object_id: ObjectID,
    ) -> Self {
        Self {
            owner,
            coin_type,
            inverted_balance,
            object_id,
        }
    }

    pub fn new(owner: SuiAddress, coin_type: String, balance: u64, object_id: ObjectID) -> Self {
        Self {
            owner,
            coin_type,
            inverted_balance: !balance,
            object_id,
        }
    }
}

const CURRENT_DB_VERSION: u64 = 0;
const _CURRENT_COIN_INDEX_VERSION: u64 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct MetadataInfo {
    /// Version of the Database
    version: u64,
    /// Version of each of the column families
    ///
    /// This is used to version individual column families to determine if a CF needs to be
    /// (re)initialized on startup.
    column_families: BTreeMap<String, ColumnFamilyInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ColumnFamilyInfo {
    version: u64,
}

pub const MAX_TX_RANGE_SIZE: u64 = 4096;

pub const MAX_GET_OWNED_OBJECT_SIZE: usize = 256;
const ENV_VAR_COIN_INDEX_BLOCK_CACHE_SIZE_MB: &str = "COIN_INDEX_BLOCK_CACHE_MB";
const ENV_VAR_DISABLE_INDEX_CACHE: &str = "DISABLE_INDEX_CACHE";
const ENV_VAR_INVALIDATE_INSTEAD_OF_UPDATE: &str = "INVALIDATE_INSTEAD_OF_UPDATE";

#[derive(Default, Copy, Clone, Debug, Eq, PartialEq)]
pub struct TotalBalance {
    pub balance: i128,
    pub num_coins: i64,
}

#[derive(Debug)]
pub struct ObjectIndexChanges {
    pub deleted_owners: Vec<OwnerIndexKey>,
    pub deleted_dynamic_fields: Vec<DynamicFieldKey>,
    pub new_owners: Vec<(OwnerIndexKey, ObjectInfo)>,
    pub new_dynamic_fields: Vec<(DynamicFieldKey, DynamicFieldInfo)>,
}

#[derive(Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub struct CoinInfo {
    pub version: SequenceNumber,
    pub digest: ObjectDigest,
    pub balance: u64,
    pub previous_transaction: TransactionDigest,
}

impl CoinInfo {
    pub fn from_object(object: &Object) -> Option<CoinInfo> {
        object.as_coin_maybe().map(|coin| CoinInfo {
            version: object.version(),
            digest: object.digest(),
            previous_transaction: object.previous_transaction,
            balance: coin.value(),
        })
    }
}

pub struct IndexStoreMetrics {
    balance_lookup_from_db: IntCounter,
    balance_lookup_from_total: IntCounter,
    all_balance_lookup_from_db: IntCounter,
    all_balance_lookup_from_total: IntCounter,
}

impl IndexStoreMetrics {
    pub fn new(registry: &Registry) -> IndexStoreMetrics {
        Self {
            balance_lookup_from_db: register_int_counter_with_registry!(
                "balance_lookup_from_db",
                "Total number of balance requests served from cache",
                registry,
            )
            .unwrap(),
            balance_lookup_from_total: register_int_counter_with_registry!(
                "balance_lookup_from_total",
                "Total number of balance requests served ",
                registry,
            )
            .unwrap(),
            all_balance_lookup_from_db: register_int_counter_with_registry!(
                "all_balance_lookup_from_db",
                "Total number of all balance requests served from cache",
                registry,
            )
            .unwrap(),
            all_balance_lookup_from_total: register_int_counter_with_registry!(
                "all_balance_lookup_from_total",
                "Total number of all balance requests served",
                registry,
            )
            .unwrap(),
        }
    }
}

pub struct IndexStoreCaches {
    per_coin_type_balance: ShardedLruCache<(SuiAddress, TypeTag), SuiResult<TotalBalance>>,
    all_balances: ShardedLruCache<SuiAddress, SuiResult<Arc<HashMap<TypeTag, TotalBalance>>>>,
    pub locks: MutexTable<SuiAddress>,
}

#[derive(Default)]
pub struct IndexStoreCacheUpdates {
    _locks: Vec<OwnedMutexGuard<()>>,
    per_coin_type_balance_changes: Vec<((SuiAddress, TypeTag), SuiResult<TotalBalance>)>,
    all_balance_changes: Vec<(SuiAddress, SuiResult<Arc<AllBalance>>)>,
}

#[derive(DBMapUtils)]
pub struct IndexStoreTables {
    /// A singleton that store metadata information on the DB.
    ///
    /// A few uses for this singleton:
    /// - determining if the DB has been initialized (as some tables could still be empty post
    ///     initialization)
    /// - version of each column family and their respective initialization status
    meta: DBMap<(), MetadataInfo>,

    /// Index from sui address to transactions initiated by that address.
    transactions_from_addr: DBMap<(SuiAddress, TxSequenceNumber), TransactionDigest>,

    /// Index from sui address to transactions that were sent to that address.
    transactions_to_addr: DBMap<(SuiAddress, TxSequenceNumber), TransactionDigest>,

    /// Index from object id to transactions that used that object id as input.
    #[deprecated]
    transactions_by_input_object_id: DBMap<(ObjectID, TxSequenceNumber), TransactionDigest>,

    /// Index from object id to transactions that modified/created that object id.
    #[deprecated]
    transactions_by_mutated_object_id: DBMap<(ObjectID, TxSequenceNumber), TransactionDigest>,

    /// Index from package id, module and function identifier to transactions that used that moce function call as input.
    transactions_by_move_function:
        DBMap<(ObjectID, String, String, TxSequenceNumber), TransactionDigest>,

    /// Ordering of all indexed transactions.
    transaction_order: DBMap<TxSequenceNumber, TransactionDigest>,

    /// Index from transaction digest to sequence number.
    transactions_seq: DBMap<TransactionDigest, TxSequenceNumber>,

    /// This is an index of object references to currently existing objects, indexed by the
    /// composite key of the SuiAddress of their owner and the object ID of the object.
    /// This composite index allows an efficient iterator to list all objected currently owned
    /// by a specific user, and their object reference.
    owner_index: DBMap<OwnerIndexKey, ObjectInfo>,

    coin_index_2: DBMap<CoinIndexKey2, CoinInfo>,

    /// This is an index of object references to currently existing dynamic field object, indexed by the
    /// composite key of the object ID of their parent and the object ID of the dynamic field object.
    /// This composite index allows an efficient iterator to list all objects currently owned
    /// by a specific object, and their object reference.
    dynamic_field_index: DBMap<DynamicFieldKey, DynamicFieldInfo>,

    event_order: DBMap<EventId, EventIndex>,
    event_by_move_module: DBMap<(ModuleId, EventId), EventIndex>,
    event_by_move_event: DBMap<(StructTag, EventId), EventIndex>,
    event_by_event_module: DBMap<(ModuleId, EventId), EventIndex>,
    event_by_sender: DBMap<(SuiAddress, EventId), EventIndex>,
    event_by_time: DBMap<(u64, EventId), EventIndex>,

    pruner_watermark: DBMap<(), TxSequenceNumber>,
}

impl IndexStoreTables {
    pub fn owner_index(&self) -> &DBMap<OwnerIndexKey, ObjectInfo> {
        &self.owner_index
    }

    pub fn coin_index(&self) -> &DBMap<CoinIndexKey2, CoinInfo> {
        &self.coin_index_2
    }

    #[allow(deprecated)]
    fn init(&mut self) -> Result<(), StorageError> {
        let metadata = {
            match self.meta.get(&()) {
                Ok(Some(metadata)) => metadata,
                Ok(None) | Err(_) => MetadataInfo {
                    version: CURRENT_DB_VERSION,
                    column_families: BTreeMap::new(),
                },
            }
        };

        // Commit to the DB that the indexes have been initialized
        self.meta.insert(&(), &metadata)?;

        Ok(())
    }
}

pub struct IndexStore {
    next_sequence_number: AtomicU64,
    tables: IndexStoreTables,
    pub caches: IndexStoreCaches,
    metrics: Arc<IndexStoreMetrics>,
    max_type_length: u64,
    remove_deprecated_tables: bool,
    pruner_watermark: Arc<AtomicU64>,
}

struct JsonRpcCompactionMetrics {
    key_removed: IntCounterVec,
    key_kept: IntCounterVec,
    key_error: IntCounterVec,
}

impl JsonRpcCompactionMetrics {
    pub fn new(registry: &Registry) -> Arc<Self> {
        Arc::new(Self {
            key_removed: register_int_counter_vec_with_registry!(
                "json_rpc_compaction_filter_key_removed",
                "Compaction key removed",
                &["cf"],
                registry
            )
            .unwrap(),
            key_kept: register_int_counter_vec_with_registry!(
                "json_rpc_compaction_filter_key_kept",
                "Compaction key kept",
                &["cf"],
                registry
            )
            .unwrap(),
            key_error: register_int_counter_vec_with_registry!(
                "json_rpc_compaction_filter_key_error",
                "Compaction error",
                &["cf"],
                registry
            )
            .unwrap(),
        })
    }
}

fn compaction_filter_config<T: DeserializeOwned>(
    name: &str,
    metrics: Arc<JsonRpcCompactionMetrics>,
    mut db_options: DBOptions,
    pruner_watermark: Arc<AtomicU64>,
    extractor: impl Fn(T) -> TxSequenceNumber + Send + Sync + 'static,
    by_key: bool,
) -> DBOptions {
    let cf = name.to_string();
    db_options
        .options
        .set_compaction_filter(name, move |_, key, value| {
            let bytes = if by_key { key } else { value };
            let deserializer = bincode::DefaultOptions::new()
                .with_big_endian()
                .with_fixint_encoding();
            match deserializer.deserialize(bytes) {
                Ok(key_data) => {
                    let sequence_number = extractor(key_data);
                    if sequence_number < pruner_watermark.load(Ordering::Relaxed) {
                        metrics.key_removed.with_label_values(&[&cf]).inc();
                        Decision::Remove
                    } else {
                        metrics.key_kept.with_label_values(&[&cf]).inc();
                        Decision::Keep
                    }
                }
                Err(_) => {
                    metrics.key_error.with_label_values(&[&cf]).inc();
                    Decision::Keep
                }
            }
        });
    db_options
}

fn compaction_filter_config_by_key<T: DeserializeOwned>(
    name: &str,
    metrics: Arc<JsonRpcCompactionMetrics>,
    db_options: DBOptions,
    pruner_watermark: Arc<AtomicU64>,
    extractor: impl Fn(T) -> TxSequenceNumber + Send + Sync + 'static,
) -> DBOptions {
    compaction_filter_config(name, metrics, db_options, pruner_watermark, extractor, true)
}

fn coin_index_table_default_config() -> DBOptions {
    default_db_options()
        .optimize_for_write_throughput()
        .optimize_for_read(
            read_size_from_env(ENV_VAR_COIN_INDEX_BLOCK_CACHE_SIZE_MB).unwrap_or(5 * 1024),
        )
        .disable_write_throttling()
}

impl IndexStore {
    pub fn new_without_init(
        path: PathBuf,
        registry: &Registry,
        max_type_length: Option<u64>,
        remove_deprecated_tables: bool,
    ) -> Self {
        let db_options = default_db_options().disable_write_throttling();
        let pruner_watermark = Arc::new(AtomicU64::new(0));
        let compaction_metrics = JsonRpcCompactionMetrics::new(registry);
        let table_options = DBMapTableConfigMap::new(BTreeMap::from([
            (
                "transactions_from_addr".to_string(),
                compaction_filter_config_by_key(
                    "transactions_from_addr",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, id): (SuiAddress, TxSequenceNumber)| id,
                ),
            ),
            (
                "transactions_to_addr".to_string(),
                compaction_filter_config_by_key(
                    "transactions_to_addr",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, sequence_number): (SuiAddress, TxSequenceNumber)| sequence_number,
                ),
            ),
            (
                "transactions_by_move_function".to_string(),
                compaction_filter_config_by_key(
                    "transactions_by_move_function",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, _, _, id): (ObjectID, String, String, TxSequenceNumber)| id,
                ),
            ),
            (
                "transaction_order".to_string(),
                compaction_filter_config_by_key(
                    "transaction_order",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |sequence_number: TxSequenceNumber| sequence_number,
                ),
            ),
            (
                "transactions_seq".to_string(),
                compaction_filter_config(
                    "transactions_seq",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |sequence_number: TxSequenceNumber| sequence_number,
                    false,
                ),
            ),
            (
                "coin_index_2".to_string(),
                coin_index_table_default_config(),
            ),
            (
                "event_order".to_string(),
                compaction_filter_config_by_key(
                    "event_order",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |event_id: EventId| event_id.0,
                ),
            ),
            (
                "event_by_move_module".to_string(),
                compaction_filter_config_by_key(
                    "event_by_move_module",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, event_id): (ModuleId, EventId)| event_id.0,
                ),
            ),
            (
                "event_by_event_module".to_string(),
                compaction_filter_config_by_key(
                    "event_by_event_module",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, event_id): (ModuleId, EventId)| event_id.0,
                ),
            ),
            (
                "event_by_sender".to_string(),
                compaction_filter_config_by_key(
                    "event_by_sender",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, event_id): (SuiAddress, EventId)| event_id.0,
                ),
            ),
            (
                "event_by_time".to_string(),
                compaction_filter_config_by_key(
                    "event_by_time",
                    compaction_metrics.clone(),
                    db_options.clone(),
                    pruner_watermark.clone(),
                    |(_, event_id): (u64, EventId)| event_id.0,
                ),
            ),
        ]));
        let tables = IndexStoreTables::open_tables_read_write_with_deprecation_option(
            path,
            MetricConf::new("index"),
            Some(db_options.options),
            Some(table_options),
            remove_deprecated_tables,
        );

        let metrics = IndexStoreMetrics::new(registry);
        let caches = IndexStoreCaches {
            per_coin_type_balance: ShardedLruCache::new(1_000_000, 1000),
            all_balances: ShardedLruCache::new(1_000_000, 1000),
            locks: MutexTable::new(128),
        };
        let next_sequence_number = tables
            .transaction_order
            .reversed_safe_iter_with_bounds(None, None)
            .expect("failed to initialize indexes")
            .next()
            .transpose()
            .expect("failed to initialize indexes")
            .map(|(seq, _)| seq + 1)
            .unwrap_or(0)
            .into();
        let pruner_watermark_value = tables
            .pruner_watermark
            .get(&())
            .expect("failed to initialize index tables")
            .unwrap_or(0);
        pruner_watermark.store(pruner_watermark_value, Ordering::Relaxed);

        Self {
            tables,
            next_sequence_number,
            caches,
            metrics: Arc::new(metrics),
            max_type_length: max_type_length.unwrap_or(128),
            remove_deprecated_tables,
            pruner_watermark,
        }
    }

    pub fn new(
        path: PathBuf,
        registry: &Registry,
        max_type_length: Option<u64>,
        remove_deprecated_tables: bool,
    ) -> Self {
        let mut store =
            Self::new_without_init(path, registry, max_type_length, remove_deprecated_tables);
        store.tables.init().unwrap();
        store
    }

    pub fn tables(&self) -> &IndexStoreTables {
        &self.tables
    }

    #[instrument(skip_all)]
    pub fn index_coin(
        &self,
        digest: &TransactionDigest,
        batch: &mut DBBatch,
        object_index_changes: &ObjectIndexChanges,
        tx_coins: Option<TxCoins>,
    ) -> SuiResult<IndexStoreCacheUpdates> {
        // In production if this code path is hit, we should expect `tx_coins` to not be None.
        // However, in many tests today we do not distinguish validator and/or fullnode, so
        // we gracefully exist here.
        if tx_coins.is_none() {
            return Ok(IndexStoreCacheUpdates::default());
        }
        // Acquire locks on changed coin owners
        let mut addresses: HashSet<SuiAddress> = HashSet::new();
        addresses.extend(
            object_index_changes
                .deleted_owners
                .iter()
                .map(|(owner, _)| *owner),
        );
        addresses.extend(
            object_index_changes
                .new_owners
                .iter()
                .map(|((owner, _), _)| *owner),
        );
        let _locks = self.caches.locks.acquire_locks(addresses.into_iter());
        let mut balance_changes: HashMap<SuiAddress, HashMap<TypeTag, TotalBalance>> =
            HashMap::new();
        // Index coin info
        let (input_coins, written_coins) = tx_coins.unwrap();

        // 1. Remove old coins from the DB by looking at the set of input coin objects
        let coin_delete_keys = input_coins
            .values()
            .filter_map(|object| {
                // only process address owned coins
                let Owner::AddressOwner(owner) = object.owner() else {
                    return None;
                };

                // only process coin types
                let (coin_type, coin) = object
                    .coin_type_maybe()
                    .and_then(|coin_type| object.as_coin_maybe().map(|coin| (coin_type, coin)))?;

                let key = CoinIndexKey2::new(
                    *owner,
                    coin_type.to_string(),
                    coin.balance.value(),
                    object.id(),
                );

                let map = balance_changes.entry(*owner).or_default();
                let entry = map.entry(coin_type).or_insert(TotalBalance {
                    num_coins: 0,
                    balance: 0,
                });
                entry.num_coins -= 1;
                entry.balance -= coin.balance.value() as i128;

                Some(key)
            })
            .collect::<Vec<_>>();
        trace!(
            tx_digset=?digest,
            "coin_delete_keys: {:?}",
            coin_delete_keys,
        );
        batch.delete_batch(&self.tables.coin_index_2, coin_delete_keys)?;

        // 2. Insert new coins, or new versions of coins, by looking at `written_coins`.
        let coin_add_keys = written_coins
            .values()
            .filter_map(|object| {
                // only process address owned coins
                let Owner::AddressOwner(owner) = object.owner() else {
                    return None;
                };

                // only process coin types
                let (coin_type, coin) = object
                    .coin_type_maybe()
                    .and_then(|coin_type| object.as_coin_maybe().map(|coin| (coin_type, coin)))?;

                let key = CoinIndexKey2::new(
                    *owner,
                    coin_type.to_string(),
                    coin.balance.value(),
                    object.id(),
                );
                let value = CoinInfo {
                    version: object.version(),
                    digest: object.digest(),
                    balance: coin.balance.value(),
                    previous_transaction: object.previous_transaction,
                };
                let map = balance_changes.entry(*owner).or_default();
                let entry = map.entry(coin_type).or_insert(TotalBalance {
                    num_coins: 0,
                    balance: 0,
                });
                entry.num_coins += 1;
                entry.balance += coin.balance.value() as i128;

                Some((key, value))
            })
            .collect::<Vec<_>>();
        trace!(
            tx_digset=?digest,
            "coin_add_keys: {:?}",
            coin_add_keys,
        );

        batch.insert_batch(&self.tables.coin_index_2, coin_add_keys)?;

        let per_coin_type_balance_changes: Vec<_> = balance_changes
            .iter()
            .flat_map(|(address, balance_map)| {
                balance_map.iter().map(|(type_tag, balance)| {
                    (
                        (*address, type_tag.clone()),
                        Ok::<TotalBalance, SuiError>(*balance),
                    )
                })
            })
            .collect();
        let all_balance_changes: Vec<_> = balance_changes
            .into_iter()
            .map(|(address, balance_map)| {
                (
                    address,
                    Ok::<Arc<HashMap<TypeTag, TotalBalance>>, SuiError>(Arc::new(balance_map)),
                )
            })
            .collect();
        let cache_updates = IndexStoreCacheUpdates {
            _locks,
            per_coin_type_balance_changes,
            all_balance_changes,
        };
        Ok(cache_updates)
    }

    #[instrument(skip_all)]
    pub fn index_tx(
        &self,
        sender: SuiAddress,
        active_inputs: impl Iterator<Item = ObjectID>,
        mutated_objects: impl Iterator<Item = (ObjectRef, Owner)> + Clone,
        move_functions: impl Iterator<Item = (ObjectID, String, String)> + Clone,
        events: &TransactionEvents,
        object_index_changes: ObjectIndexChanges,
        digest: &TransactionDigest,
        timestamp_ms: u64,
        tx_coins: Option<TxCoins>,
    ) -> SuiResult<u64> {
        let sequence = self.next_sequence_number.fetch_add(1, Ordering::SeqCst);
        let mut batch = self.tables.transactions_from_addr.batch();

        batch.insert_batch(
            &self.tables.transaction_order,
            std::iter::once((sequence, *digest)),
        )?;

        batch.insert_batch(
            &self.tables.transactions_seq,
            std::iter::once((*digest, sequence)),
        )?;

        batch.insert_batch(
            &self.tables.transactions_from_addr,
            std::iter::once(((sender, sequence), *digest)),
        )?;

        #[allow(deprecated)]
        if !self.remove_deprecated_tables {
            batch.insert_batch(
                &self.tables.transactions_by_input_object_id,
                active_inputs.map(|id| ((id, sequence), *digest)),
            )?;

            batch.insert_batch(
                &self.tables.transactions_by_mutated_object_id,
                mutated_objects
                    .clone()
                    .map(|(obj_ref, _)| ((obj_ref.0, sequence), *digest)),
            )?;
        }

        batch.insert_batch(
            &self.tables.transactions_by_move_function,
            move_functions
                .map(|(obj_id, module, function)| ((obj_id, module, function, sequence), *digest)),
        )?;

        batch.insert_batch(
            &self.tables.transactions_to_addr,
            mutated_objects.filter_map(|(_, owner)| {
                owner
                    .get_address_owner_address()
                    .ok()
                    .map(|addr| ((addr, sequence), digest))
            }),
        )?;

        // Coin Index
        let cache_updates = self.index_coin(digest, &mut batch, &object_index_changes, tx_coins)?;

        // Owner index
        batch.delete_batch(
            &self.tables.owner_index,
            object_index_changes.deleted_owners.into_iter(),
        )?;
        batch.delete_batch(
            &self.tables.dynamic_field_index,
            object_index_changes.deleted_dynamic_fields.into_iter(),
        )?;

        batch.insert_batch(
            &self.tables.owner_index,
            object_index_changes.new_owners.into_iter(),
        )?;

        batch.insert_batch(
            &self.tables.dynamic_field_index,
            object_index_changes.new_dynamic_fields.into_iter(),
        )?;

        // events
        let event_digest = events.digest();
        batch.insert_batch(
            &self.tables.event_order,
            events
                .data
                .iter()
                .enumerate()
                .map(|(i, _)| ((sequence, i), (event_digest, *digest, timestamp_ms))),
        )?;
        batch.insert_batch(
            &self.tables.event_by_move_module,
            events
                .data
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    (
                        i,
                        ModuleId::new(e.package_id.into(), e.transaction_module.clone()),
                    )
                })
                .map(|(i, m)| ((m, (sequence, i)), (event_digest, *digest, timestamp_ms))),
        )?;
        batch.insert_batch(
            &self.tables.event_by_sender,
            events.data.iter().enumerate().map(|(i, e)| {
                (
                    (e.sender, (sequence, i)),
                    (event_digest, *digest, timestamp_ms),
                )
            }),
        )?;
        batch.insert_batch(
            &self.tables.event_by_move_event,
            events.data.iter().enumerate().map(|(i, e)| {
                (
                    (e.type_.clone(), (sequence, i)),
                    (event_digest, *digest, timestamp_ms),
                )
            }),
        )?;

        batch.insert_batch(
            &self.tables.event_by_time,
            events.data.iter().enumerate().map(|(i, _)| {
                (
                    (timestamp_ms, (sequence, i)),
                    (event_digest, *digest, timestamp_ms),
                )
            }),
        )?;

        batch.insert_batch(
            &self.tables.event_by_event_module,
            events.data.iter().enumerate().map(|(i, e)| {
                (
                    (
                        ModuleId::new(e.type_.address, e.type_.module.clone()),
                        (sequence, i),
                    ),
                    (event_digest, *digest, timestamp_ms),
                )
            }),
        )?;

        let invalidate_caches =
            read_size_from_env(ENV_VAR_INVALIDATE_INSTEAD_OF_UPDATE).unwrap_or(0) > 0;

        if invalidate_caches {
            // Invalidate cache before writing to db so we always serve latest values
            self.invalidate_per_coin_type_cache(
                cache_updates
                    .per_coin_type_balance_changes
                    .iter()
                    .map(|x| x.0.clone()),
            )?;
            self.invalidate_all_balance_cache(
                cache_updates.all_balance_changes.iter().map(|x| x.0),
            )?;
        }

        batch.write()?;

        if !invalidate_caches {
            // We cannot update the cache before updating the db or else on failing to write to db
            // we will update the cache twice). However, this only means cache is eventually consistent with
            // the db (within a very short delay)
            self.update_per_coin_type_cache(cache_updates.per_coin_type_balance_changes)?;
            self.update_all_balance_cache(cache_updates.all_balance_changes)?;
        }
        Ok(sequence)
    }

    pub fn next_sequence_number(&self) -> TxSequenceNumber {
        self.next_sequence_number.load(Ordering::SeqCst) + 1
    }

    #[instrument(skip(self))]
    pub fn get_transactions(
        &self,
        filter: Option<TransactionFilter>,
        cursor: Option<TransactionDigest>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        // Lookup TransactionDigest sequence number,
        let cursor = if let Some(cursor) = cursor {
            Some(
                self.get_transaction_seq(&cursor)?
                    .ok_or(SuiError::TransactionNotFound { digest: cursor })?,
            )
        } else {
            None
        };
        match filter {
            Some(TransactionFilter::MoveFunction {
                package,
                module,
                function,
            }) => Ok(self.get_transactions_by_move_function(
                package, module, function, cursor, limit, reverse,
            )?),
            Some(TransactionFilter::InputObject(object_id)) => {
                Ok(self.get_transactions_by_input_object(object_id, cursor, limit, reverse)?)
            }
            Some(TransactionFilter::ChangedObject(object_id)) => {
                Ok(self.get_transactions_by_mutated_object(object_id, cursor, limit, reverse)?)
            }
            Some(TransactionFilter::FromAddress(address)) => {
                Ok(self.get_transactions_from_addr(address, cursor, limit, reverse)?)
            }
            Some(TransactionFilter::ToAddress(address)) => {
                Ok(self.get_transactions_to_addr(address, cursor, limit, reverse)?)
            }
            // NOTE: filter via checkpoint sequence number is implemented in
            // `get_transactions` of authority.rs.
            Some(_) => Err(SuiError::UserInputError {
                error: UserInputError::Unsupported(format!("{:?}", filter)),
            }),
            None => {
                if reverse {
                    let iter = self
                        .tables
                        .transaction_order
                        .reversed_safe_iter_with_bounds(
                            None,
                            Some(cursor.unwrap_or(TxSequenceNumber::MAX)),
                        )?
                        .skip(usize::from(cursor.is_some()))
                        .map(|result| result.map(|(_, digest)| digest));
                    if let Some(limit) = limit {
                        Ok(iter.take(limit).collect::<Result<Vec<_>, _>>()?)
                    } else {
                        Ok(iter.collect::<Result<Vec<_>, _>>()?)
                    }
                } else {
                    let iter = self
                        .tables
                        .transaction_order
                        .safe_iter_with_bounds(Some(cursor.unwrap_or(TxSequenceNumber::MIN)), None)
                        .skip(usize::from(cursor.is_some()))
                        .map(|result| result.map(|(_, digest)| digest));
                    if let Some(limit) = limit {
                        Ok(iter.take(limit).collect::<Result<Vec<_>, _>>()?)
                    } else {
                        Ok(iter.collect::<Result<Vec<_>, _>>()?)
                    }
                }
            }
        }
    }

    #[instrument(skip_all)]
    fn get_transactions_from_index<KeyT: Clone + Serialize + DeserializeOwned + PartialEq>(
        index: &DBMap<(KeyT, TxSequenceNumber), TransactionDigest>,
        key: KeyT,
        cursor: Option<TxSequenceNumber>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        Ok(if reverse {
            let iter = index
                .reversed_safe_iter_with_bounds(
                    None,
                    Some((key.clone(), cursor.unwrap_or(TxSequenceNumber::MAX))),
                )?
                // skip one more if exclusive cursor is Some
                .skip(usize::from(cursor.is_some()))
                .take_while(|result| {
                    result
                        .as_ref()
                        .map(|((id, _), _)| *id == key)
                        .unwrap_or(false)
                })
                .map(|result| result.map(|(_, digest)| digest));
            if let Some(limit) = limit {
                iter.take(limit).collect::<Result<Vec<_>, _>>()?
            } else {
                iter.collect::<Result<Vec<_>, _>>()?
            }
        } else {
            let iter = index
                .safe_iter_with_bounds(
                    Some((key.clone(), cursor.unwrap_or(TxSequenceNumber::MIN))),
                    None,
                )
                // skip one more if exclusive cursor is Some
                .skip(usize::from(cursor.is_some()))
                .map(|result| result.expect("iterator db error"))
                .take_while(|((id, _), _)| *id == key)
                .map(|(_, digest)| digest);
            if let Some(limit) = limit {
                iter.take(limit).collect()
            } else {
                iter.collect()
            }
        })
    }

    #[instrument(skip(self))]
    pub fn get_transactions_by_input_object(
        &self,
        input_object: ObjectID,
        cursor: Option<TxSequenceNumber>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        if self.remove_deprecated_tables {
            return Ok(vec![]);
        }
        #[allow(deprecated)]
        Self::get_transactions_from_index(
            &self.tables.transactions_by_input_object_id,
            input_object,
            cursor,
            limit,
            reverse,
        )
    }

    #[instrument(skip(self))]
    pub fn get_transactions_by_mutated_object(
        &self,
        mutated_object: ObjectID,
        cursor: Option<TxSequenceNumber>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        if self.remove_deprecated_tables {
            return Ok(vec![]);
        }
        #[allow(deprecated)]
        Self::get_transactions_from_index(
            &self.tables.transactions_by_mutated_object_id,
            mutated_object,
            cursor,
            limit,
            reverse,
        )
    }

    #[instrument(skip(self))]
    pub fn get_transactions_from_addr(
        &self,
        addr: SuiAddress,
        cursor: Option<TxSequenceNumber>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        Self::get_transactions_from_index(
            &self.tables.transactions_from_addr,
            addr,
            cursor,
            limit,
            reverse,
        )
    }

    #[instrument(skip(self))]
    pub fn get_transactions_by_move_function(
        &self,
        package: ObjectID,
        module: Option<String>,
        function: Option<String>,
        cursor: Option<TxSequenceNumber>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        // If we are passed a function with no module return a UserInputError
        if function.is_some() && module.is_none() {
            return Err(SuiError::UserInputError {
                error: UserInputError::MoveFunctionInputError(
                    "Cannot supply function without supplying module".to_string(),
                ),
            });
        }

        // We cannot have a cursor without filling out the other keys.
        if cursor.is_some() && (module.is_none() || function.is_none()) {
            return Err(SuiError::UserInputError {
                error: UserInputError::MoveFunctionInputError(
                    "Cannot supply cursor without supplying module and function".to_string(),
                ),
            });
        }

        let cursor_val = cursor.unwrap_or(if reverse {
            TxSequenceNumber::MAX
        } else {
            TxSequenceNumber::MIN
        });

        let max_string = "Z".repeat(self.max_type_length.try_into().unwrap());
        let module_val = module.clone().unwrap_or(if reverse {
            max_string.clone()
        } else {
            "".to_string()
        });

        let function_val =
            function
                .clone()
                .unwrap_or(if reverse { max_string } else { "".to_string() });

        let key = (package, module_val, function_val, cursor_val);
        Ok(if reverse {
            let iter = self
                .tables
                .transactions_by_move_function
                .reversed_safe_iter_with_bounds(None, Some(key))?
                // skip one more if exclusive cursor is Some
                .skip(usize::from(cursor.is_some()))
                .take_while(|result| {
                    result
                        .as_ref()
                        .map(|((id, m, f, _), _)| {
                            *id == package
                                && module.as_ref().map(|x| x == m).unwrap_or(true)
                                && function.as_ref().map(|x| x == f).unwrap_or(true)
                        })
                        .unwrap_or(false)
                })
                .map(|result| result.map(|(_, digest)| digest));
            if let Some(limit) = limit {
                iter.take(limit).collect::<Result<Vec<_>, _>>()?
            } else {
                iter.collect::<Result<Vec<_>, _>>()?
            }
        } else {
            let iter = self
                .tables
                .transactions_by_move_function
                .safe_iter_with_bounds(Some(key), None)
                .map(|result| result.expect("iterator db error"))
                // skip one more if exclusive cursor is Some
                .skip(usize::from(cursor.is_some()))
                .take_while(|((id, m, f, _), _)| {
                    *id == package
                        && module.as_ref().map(|x| x == m).unwrap_or(true)
                        && function.as_ref().map(|x| x == f).unwrap_or(true)
                })
                .map(|(_, digest)| digest);
            if let Some(limit) = limit {
                iter.take(limit).collect()
            } else {
                iter.collect()
            }
        })
    }

    #[instrument(skip(self))]
    pub fn get_transactions_to_addr(
        &self,
        addr: SuiAddress,
        cursor: Option<TxSequenceNumber>,
        limit: Option<usize>,
        reverse: bool,
    ) -> SuiResult<Vec<TransactionDigest>> {
        Self::get_transactions_from_index(
            &self.tables.transactions_to_addr,
            addr,
            cursor,
            limit,
            reverse,
        )
    }

    #[instrument(skip(self))]
    pub fn get_transaction_seq(
        &self,
        digest: &TransactionDigest,
    ) -> SuiResult<Option<TxSequenceNumber>> {
        Ok(self.tables.transactions_seq.get(digest)?)
    }

    #[instrument(skip(self))]
    pub fn all_events(
        &self,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Ok(if descending {
            self.tables
                .event_order
                .reversed_safe_iter_with_bounds(None, Some((tx_seq, event_seq)))?
                .take(limit)
                .map(|result| {
                    result.map(|((_, event_seq), (digest, tx_digest, time))| {
                        (digest, tx_digest, event_seq, time)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            self.tables
                .event_order
                .safe_iter_with_bounds(Some((tx_seq, event_seq)), None)
                .take(limit)
                .map(|result| {
                    result.map(|((_, event_seq), (digest, tx_digest, time))| {
                        (digest, tx_digest, event_seq, time)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        })
    }

    #[instrument(skip(self))]
    pub fn events_by_transaction(
        &self,
        digest: &TransactionDigest,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        let seq = self
            .get_transaction_seq(digest)?
            .ok_or(SuiError::TransactionNotFound { digest: *digest })?;
        Ok(if descending {
            self.tables
                .event_order
                .reversed_safe_iter_with_bounds(None, Some((min(tx_seq, seq), event_seq)))?
                .take_while(|result| {
                    result
                        .as_ref()
                        .map(|((tx, _), _)| tx == &seq)
                        .unwrap_or(false)
                })
                .take(limit)
                .map(|result| {
                    result.map(|((_, event_seq), (digest, tx_digest, time))| {
                        (digest, tx_digest, event_seq, time)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            self.tables
                .event_order
                .safe_iter_with_bounds(Some((max(tx_seq, seq), event_seq)), None)
                .map(|result| result.expect("iterator db error"))
                .take_while(|((tx, _), _)| tx == &seq)
                .take(limit)
                .map(|((_, event_seq), (digest, tx_digest, time))| {
                    (digest, tx_digest, event_seq, time)
                })
                .collect()
        })
    }

    #[instrument(skip_all)]
    fn get_event_from_index<KeyT: Clone + PartialEq + Serialize + DeserializeOwned>(
        index: &DBMap<(KeyT, EventId), (TransactionEventsDigest, TransactionDigest, u64)>,
        key: &KeyT,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Ok(if descending {
            index
                .reversed_safe_iter_with_bounds(None, Some((key.clone(), (tx_seq, event_seq))))?
                .take_while(|result| result.as_ref().map(|((m, _), _)| m == key).unwrap_or(false))
                .take(limit)
                .map(|result| {
                    result.map(|((_, (_, event_seq)), (digest, tx_digest, time))| {
                        (digest, tx_digest, event_seq, time)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            index
                .safe_iter_with_bounds(Some((key.clone(), (tx_seq, event_seq))), None)
                .map(|result| result.expect("iterator db error"))
                .take_while(|((m, _), _)| m == key)
                .take(limit)
                .map(|((_, (_, event_seq)), (digest, tx_digest, time))| {
                    (digest, tx_digest, event_seq, time)
                })
                .collect()
        })
    }

    #[instrument(skip(self))]
    pub fn events_by_module_id(
        &self,
        module: &ModuleId,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Self::get_event_from_index(
            &self.tables.event_by_move_module,
            module,
            tx_seq,
            event_seq,
            limit,
            descending,
        )
    }

    #[instrument(skip(self))]
    pub fn events_by_move_event_struct_name(
        &self,
        struct_name: &StructTag,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Self::get_event_from_index(
            &self.tables.event_by_move_event,
            struct_name,
            tx_seq,
            event_seq,
            limit,
            descending,
        )
    }

    #[instrument(skip(self))]
    pub fn events_by_move_event_module(
        &self,
        module_id: &ModuleId,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Self::get_event_from_index(
            &self.tables.event_by_event_module,
            module_id,
            tx_seq,
            event_seq,
            limit,
            descending,
        )
    }

    #[instrument(skip(self))]
    pub fn events_by_sender(
        &self,
        sender: &SuiAddress,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Self::get_event_from_index(
            &self.tables.event_by_sender,
            sender,
            tx_seq,
            event_seq,
            limit,
            descending,
        )
    }

    #[instrument(skip(self))]
    pub fn event_iterator(
        &self,
        start_time: u64,
        end_time: u64,
        tx_seq: TxSequenceNumber,
        event_seq: usize,
        limit: usize,
        descending: bool,
    ) -> SuiResult<Vec<(TransactionEventsDigest, TransactionDigest, usize, u64)>> {
        Ok(if descending {
            self.tables
                .event_by_time
                .reversed_safe_iter_with_bounds(None, Some((end_time, (tx_seq, event_seq))))?
                .take_while(|result| {
                    result
                        .as_ref()
                        .map(|((m, _), _)| m >= &start_time)
                        .unwrap_or(false)
                })
                .take(limit)
                .map(|result| {
                    result.map(|((_, (_, event_seq)), (digest, tx_digest, time))| {
                        (digest, tx_digest, event_seq, time)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            self.tables
                .event_by_time
                .safe_iter_with_bounds(Some((start_time, (tx_seq, event_seq))), None)
                .map(|result| result.expect("iterator db error"))
                .take_while(|((m, _), _)| m <= &end_time)
                .take(limit)
                .map(|((_, (_, event_seq)), (digest, tx_digest, time))| {
                    (digest, tx_digest, event_seq, time)
                })
                .collect()
        })
    }

    pub fn prune(&self, cut_time_ms: u64) -> SuiResult<TxSequenceNumber> {
        match self
            .tables
            .event_by_time
            .reversed_safe_iter_with_bounds(
                None,
                Some((cut_time_ms, (TxSequenceNumber::MAX, usize::MAX))),
            )?
            .next()
            .transpose()?
        {
            Some(((_, (watermark, _)), _)) => {
                if let Some(digest) = self.tables.transaction_order.get(&watermark)? {
                    info!(
                        "json rpc index pruning. Watermark is {} with digest {}",
                        watermark, digest
                    );
                }
                self.pruner_watermark.store(watermark, Ordering::Relaxed);
                self.tables.pruner_watermark.insert(&(), &watermark)?;
                Ok(watermark)
            }
            None => Ok(0),
        }
    }

    pub fn get_dynamic_fields_iterator(
        &self,
        object: ObjectID,
        cursor: Option<ObjectID>,
    ) -> SuiResult<impl Iterator<Item = Result<(ObjectID, DynamicFieldInfo), TypedStoreError>> + '_>
    {
        debug!(?object, "get_dynamic_fields");
        // The object id 0 is the smallest possible
        let iter_lower_bound = (object, cursor.unwrap_or(ObjectID::ZERO));
        let iter_upper_bound = (object, ObjectID::MAX);
        Ok(self
            .tables
            .dynamic_field_index
            .safe_iter_with_bounds(Some(iter_lower_bound), Some(iter_upper_bound))
            // skip an extra b/c the cursor is exclusive
            .skip(usize::from(cursor.is_some()))
            .take_while(move |result| result.is_err() || (result.as_ref().unwrap().0 .0 == object))
            .map_ok(|((_, c), object_info)| (c, object_info)))
    }

    #[instrument(skip(self))]
    pub fn get_dynamic_field_object_id(
        &self,
        object: ObjectID,
        name_type: TypeTag,
        name_bcs_bytes: &[u8],
    ) -> SuiResult<Option<ObjectID>> {
        debug!(?object, "get_dynamic_field_object_id");
        let dynamic_field_id =
            dynamic_field::derive_dynamic_field_id(object, &name_type, name_bcs_bytes).map_err(
                |e| {
                    SuiError::Unknown(format!(
                        "Unable to generate dynamic field id. Got error: {e:?}"
                    ))
                },
            )?;

        if let Some(info) = self
            .tables
            .dynamic_field_index
            .get(&(object, dynamic_field_id))?
        {
            // info.object_id != dynamic_field_id ==> is_wrapper
            debug_assert!(
                info.object_id == dynamic_field_id
                    || matches!(name_type, TypeTag::Struct(tag) if DynamicFieldInfo::is_dynamic_object_field_wrapper(&tag))
            );
            return Ok(Some(info.object_id));
        }

        let dynamic_object_field_struct = DynamicFieldInfo::dynamic_object_field_wrapper(name_type);
        let dynamic_object_field_type = TypeTag::Struct(Box::new(dynamic_object_field_struct));
        let dynamic_object_field_id = dynamic_field::derive_dynamic_field_id(
            object,
            &dynamic_object_field_type,
            name_bcs_bytes,
        )
        .map_err(|e| {
            SuiError::Unknown(format!(
                "Unable to generate dynamic field id. Got error: {e:?}"
            ))
        })?;
        if let Some(info) = self
            .tables
            .dynamic_field_index
            .get(&(object, dynamic_object_field_id))?
        {
            return Ok(Some(info.object_id));
        }

        Ok(None)
    }

    #[instrument(skip(self))]
    pub fn get_owner_objects(
        &self,
        owner: SuiAddress,
        cursor: Option<ObjectID>,
        limit: usize,
        filter: Option<SuiObjectDataFilter>,
    ) -> SuiResult<Vec<ObjectInfo>> {
        let cursor = match cursor {
            Some(cursor) => cursor,
            None => ObjectID::ZERO,
        };
        Ok(self
            .get_owner_objects_iterator(owner, cursor, filter)?
            .take(limit)
            .collect())
    }

    pub fn get_owned_coins_iterator(
        coin_index: &DBMap<CoinIndexKey2, CoinInfo>,
        owner: SuiAddress,
        coin_type_tag: Option<String>,
    ) -> SuiResult<impl Iterator<Item = (CoinIndexKey2, CoinInfo)> + '_> {
        let all_coins = coin_type_tag.is_none();
        let starting_coin_type =
            coin_type_tag.unwrap_or_else(|| String::from_utf8([0u8].to_vec()).unwrap());
        let start_key =
            CoinIndexKey2::new(owner, starting_coin_type.clone(), u64::MAX, ObjectID::ZERO);
        Ok(coin_index
            .safe_iter_with_bounds(Some(start_key), None)
            .map(|result| result.expect("iterator db error"))
            .take_while(move |(key, _)| {
                if key.owner != owner {
                    return false;
                }
                if !all_coins && starting_coin_type != key.coin_type {
                    return false;
                }
                true
            }))
    }

    pub fn get_owned_coins_iterator_with_cursor(
        &self,
        owner: SuiAddress,
        cursor: (String, u64, ObjectID),
        limit: usize,
        one_coin_type_only: bool,
    ) -> SuiResult<impl Iterator<Item = (CoinIndexKey2, CoinInfo)> + '_> {
        let (starting_coin_type, inverted_balance, starting_object_id) = cursor;
        let start_key = CoinIndexKey2::new_from_cursor(
            owner,
            starting_coin_type.clone(),
            inverted_balance,
            starting_object_id,
        );
        Ok(self
            .tables
            .coin_index_2
            .safe_iter_with_bounds(Some(start_key), None)
            .map(|result| result.expect("iterator db error"))
            .filter(move |(key, _)| key.object_id != starting_object_id)
            .enumerate()
            .take_while(move |(index, (key, _))| {
                if *index >= limit {
                    return false;
                }
                if key.owner != owner {
                    return false;
                }
                if one_coin_type_only && starting_coin_type != key.coin_type {
                    return false;
                }
                true
            })
            .map(|(_index, (key, info))| (key, info)))
    }

    /// starting_object_id can be used to implement pagination, where a client remembers the last
    /// object id of each page, and use it to query the next page.
    pub fn get_owner_objects_iterator(
        &self,
        owner: SuiAddress,
        starting_object_id: ObjectID,
        filter: Option<SuiObjectDataFilter>,
    ) -> SuiResult<impl Iterator<Item = ObjectInfo> + '_> {
        Ok(self
            .tables
            .owner_index
            // The object id 0 is the smallest possible
            .safe_iter_with_bounds(Some((owner, starting_object_id)), None)
            .map(|result| result.expect("iterator db error"))
            .skip(usize::from(starting_object_id != ObjectID::ZERO))
            .take_while(move |((address_owner, _), _)| address_owner == &owner)
            .filter(move |(_, o)| {
                if let Some(filter) = filter.as_ref() {
                    filter.matches(o)
                } else {
                    true
                }
            })
            .map(|(_, object_info)| object_info))
    }

    pub fn insert_genesis_objects(&self, object_index_changes: ObjectIndexChanges) -> SuiResult {
        let mut batch = self.tables.owner_index.batch();
        batch.insert_batch(
            &self.tables.owner_index,
            object_index_changes.new_owners.into_iter(),
        )?;
        batch.insert_batch(
            &self.tables.dynamic_field_index,
            object_index_changes.new_dynamic_fields.into_iter(),
        )?;
        batch.write()?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.tables.owner_index.is_empty()
    }

    pub fn checkpoint_db(&self, path: &Path) -> SuiResult {
        // We are checkpointing the whole db
        self.tables
            .transactions_from_addr
            .checkpoint_db(path)
            .map_err(Into::into)
    }

    /// This method first gets the balance from `per_coin_type_balance` cache. On a cache miss, it
    /// gets the balance for passed in `coin_type` from the `all_balance` cache. Only on the second
    /// cache miss, we go to the database (expensive) and update the cache. Notice that db read is
    /// done with `spawn_blocking` as that is expected to block
    #[instrument(skip(self))]
    pub fn get_balance(&self, owner: SuiAddress, coin_type: TypeTag) -> SuiResult<TotalBalance> {
        let force_disable_cache = read_size_from_env(ENV_VAR_DISABLE_INDEX_CACHE).unwrap_or(0) > 0;
        let cloned_coin_type = coin_type.clone();
        let metrics_cloned = self.metrics.clone();
        let coin_index_cloned = self.tables.coin_index_2.clone();
        if force_disable_cache {
            Self::get_balance_from_db(metrics_cloned, coin_index_cloned, owner, cloned_coin_type)
                .map_err(|e| {
                SuiError::ExecutionError(format!("Failed to read balance frm DB: {:?}", e))
            })?;
        }

        self.metrics.balance_lookup_from_total.inc();

        let balance = self
            .caches
            .per_coin_type_balance
            .get(&(owner, coin_type.clone()));
        if let Some(balance) = balance {
            return balance;
        }
        // cache miss, lookup in all balance cache
        let all_balance = self.caches.all_balances.get(&owner.clone());
        if let Some(Ok(all_balance)) = all_balance {
            if let Some(balance) = all_balance.get(&coin_type) {
                return Ok(*balance);
            }
        }
        let cloned_coin_type = coin_type.clone();
        let metrics_cloned = self.metrics.clone();
        let coin_index_cloned = self.tables.coin_index_2.clone();
        self.caches
            .per_coin_type_balance
            .get_with((owner, coin_type), move || {
                Self::get_balance_from_db(
                    metrics_cloned,
                    coin_index_cloned,
                    owner,
                    cloned_coin_type,
                )
                .map_err(|e| {
                    SuiError::ExecutionError(format!("Failed to read balance frm DB: {:?}", e))
                })
            })
    }

    /// This method gets the balance for all coin types from the `all_balance` cache. On a cache miss,
    /// we go to the database (expensive) and update the cache. This cache is dual purpose in the
    /// sense that it not only serves `get_AllBalance()` calls but is also used for serving
    /// `get_Balance()` queries. Notice that db read is performed with `spawn_blocking` as that is
    /// expected to block
    #[instrument(skip(self))]
    pub fn get_all_balance(
        &self,
        owner: SuiAddress,
    ) -> SuiResult<Arc<HashMap<TypeTag, TotalBalance>>> {
        let force_disable_cache = read_size_from_env(ENV_VAR_DISABLE_INDEX_CACHE).unwrap_or(0) > 0;
        let metrics_cloned = self.metrics.clone();
        let coin_index_cloned = self.tables.coin_index_2.clone();
        if force_disable_cache {
            Self::get_all_balances_from_db(metrics_cloned, coin_index_cloned, owner).map_err(
                |e| {
                    SuiError::ExecutionError(format!("Failed to read all balance from DB: {:?}", e))
                },
            )?;
        }

        self.metrics.all_balance_lookup_from_total.inc();
        let metrics_cloned = self.metrics.clone();
        let coin_index_cloned = self.tables.coin_index_2.clone();
        self.caches.all_balances.get_with(owner, move || {
            Self::get_all_balances_from_db(metrics_cloned, coin_index_cloned, owner).map_err(|e| {
                SuiError::ExecutionError(format!("Failed to read all balance from DB: {:?}", e))
            })
        })
    }

    /// Read balance for a `SuiAddress` and `CoinType` from the backend database
    #[instrument(skip_all)]
    pub fn get_balance_from_db(
        metrics: Arc<IndexStoreMetrics>,
        coin_index: DBMap<CoinIndexKey2, CoinInfo>,
        owner: SuiAddress,
        coin_type: TypeTag,
    ) -> SuiResult<TotalBalance> {
        metrics.balance_lookup_from_db.inc();
        let coin_type_str = coin_type.to_string();
        let coins =
            Self::get_owned_coins_iterator(&coin_index, owner, Some(coin_type_str.clone()))?;

        let mut balance = 0i128;
        let mut num_coins = 0;
        for (_key, coin_info) in coins {
            balance += coin_info.balance as i128;
            num_coins += 1;
        }
        Ok(TotalBalance { balance, num_coins })
    }

    /// Read all balances for a `SuiAddress` from the backend database
    #[instrument(skip_all)]
    pub fn get_all_balances_from_db(
        metrics: Arc<IndexStoreMetrics>,
        coin_index: DBMap<CoinIndexKey2, CoinInfo>,
        owner: SuiAddress,
    ) -> SuiResult<Arc<HashMap<TypeTag, TotalBalance>>> {
        metrics.all_balance_lookup_from_db.inc();
        let mut balances: HashMap<TypeTag, TotalBalance> = HashMap::new();
        let coins = Self::get_owned_coins_iterator(&coin_index, owner, None)?
            .chunk_by(|(key, _coin)| key.coin_type.clone());
        for (coin_type, coins) in &coins {
            let mut total_balance = 0i128;
            let mut coin_object_count = 0;
            for (_, coin_info) in coins {
                total_balance += coin_info.balance as i128;
                coin_object_count += 1;
            }
            let coin_type =
                TypeTag::Struct(Box::new(parse_sui_struct_tag(&coin_type).map_err(|e| {
                    SuiError::ExecutionError(format!(
                        "Failed to parse event sender address: {:?}",
                        e
                    ))
                })?));
            balances.insert(
                coin_type,
                TotalBalance {
                    num_coins: coin_object_count,
                    balance: total_balance,
                },
            );
        }
        Ok(Arc::new(balances))
    }

    fn invalidate_per_coin_type_cache(
        &self,
        keys: impl IntoIterator<Item = (SuiAddress, TypeTag)>,
    ) -> SuiResult {
        self.caches.per_coin_type_balance.batch_invalidate(keys);
        Ok(())
    }

    fn invalidate_all_balance_cache(
        &self,
        addresses: impl IntoIterator<Item = SuiAddress>,
    ) -> SuiResult {
        self.caches.all_balances.batch_invalidate(addresses);
        Ok(())
    }

    fn update_per_coin_type_cache(
        &self,
        keys: impl IntoIterator<Item = ((SuiAddress, TypeTag), SuiResult<TotalBalance>)>,
    ) -> SuiResult {
        self.caches
            .per_coin_type_balance
            .batch_merge(keys, Self::merge_balance);
        Ok(())
    }

    fn merge_balance(
        old_balance: &SuiResult<TotalBalance>,
        balance_delta: &SuiResult<TotalBalance>,
    ) -> SuiResult<TotalBalance> {
        if let Ok(old_balance) = old_balance {
            if let Ok(balance_delta) = balance_delta {
                Ok(TotalBalance {
                    balance: old_balance.balance + balance_delta.balance,
                    num_coins: old_balance.num_coins + balance_delta.num_coins,
                })
            } else {
                balance_delta.clone()
            }
        } else {
            old_balance.clone()
        }
    }

    fn update_all_balance_cache(
        &self,
        keys: impl IntoIterator<Item = (SuiAddress, SuiResult<Arc<HashMap<TypeTag, TotalBalance>>>)>,
    ) -> SuiResult {
        self.caches
            .all_balances
            .batch_merge(keys, Self::merge_all_balance);
        Ok(())
    }

    fn merge_all_balance(
        old_balance: &SuiResult<Arc<HashMap<TypeTag, TotalBalance>>>,
        balance_delta: &SuiResult<Arc<HashMap<TypeTag, TotalBalance>>>,
    ) -> SuiResult<Arc<HashMap<TypeTag, TotalBalance>>> {
        if let Ok(old_balance) = old_balance {
            if let Ok(balance_delta) = balance_delta {
                let mut new_balance = HashMap::new();
                for (key, value) in old_balance.iter() {
                    new_balance.insert(key.clone(), *value);
                }
                for (key, delta) in balance_delta.iter() {
                    let old = new_balance.entry(key.clone()).or_insert(TotalBalance {
                        balance: 0,
                        num_coins: 0,
                    });
                    let new_total = TotalBalance {
                        balance: old.balance + delta.balance,
                        num_coins: old.num_coins + delta.num_coins,
                    };
                    new_balance.insert(key.clone(), new_total);
                }
                Ok(Arc::new(new_balance))
            } else {
                balance_delta.clone()
            }
        } else {
            old_balance.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IndexStore;
    use super::ObjectIndexChanges;
    use move_core_types::account_address::AccountAddress;
    use prometheus::Registry;
    use std::collections::BTreeMap;
    use std::env::temp_dir;
    use sui_types::base_types::{ObjectInfo, ObjectType, SuiAddress};
    use sui_types::digests::TransactionDigest;
    use sui_types::effects::TransactionEvents;
    use sui_types::gas_coin::GAS;
    use sui_types::object;
    use sui_types::object::Owner;

    #[tokio::test]
    async fn test_index_cache() -> anyhow::Result<()> {
        // This test is going to invoke `index_tx()`where 10 coins each with balance 100
        // are going to be added to an address. The balance is then going to be read from the db
        // and the cache. It should be 1000. Then, we are going to delete 3 of those coins from
        // the address and invoke `index_tx()` again and read balance. The balance should be 700
        // and verified from both db and cache.
        // This tests make sure we are invalidating entries in the cache and always reading latest
        // balance.
        let index_store =
            IndexStore::new_without_init(temp_dir(), &Registry::default(), Some(128), false);
        let address: SuiAddress = AccountAddress::random().into();
        let mut written_objects = BTreeMap::new();
        let mut input_objects = BTreeMap::new();
        let mut object_map = BTreeMap::new();

        let mut new_objects = vec![];
        for _i in 0..10 {
            let object = object::Object::new_gas_with_balance_and_owner_for_testing(100, address);
            new_objects.push((
                (address, object.id()),
                ObjectInfo {
                    object_id: object.id(),
                    version: object.version(),
                    digest: object.digest(),
                    type_: ObjectType::Struct(object.type_().unwrap().clone()),
                    owner: Owner::AddressOwner(address),
                    previous_transaction: object.previous_transaction,
                },
            ));
            object_map.insert(object.id(), object.clone());
            written_objects.insert(object.data.id(), object);
        }
        let object_index_changes = ObjectIndexChanges {
            deleted_owners: vec![],
            deleted_dynamic_fields: vec![],
            new_owners: new_objects,
            new_dynamic_fields: vec![],
        };

        let tx_coins = (input_objects.clone(), written_objects.clone());
        index_store.index_tx(
            address,
            vec![].into_iter(),
            vec![].into_iter(),
            vec![].into_iter(),
            &TransactionEvents { data: vec![] },
            object_index_changes,
            &TransactionDigest::random(),
            1234,
            Some(tx_coins),
        )?;

        let balance_from_db = IndexStore::get_balance_from_db(
            index_store.metrics.clone(),
            index_store.tables.coin_index_2.clone(),
            address,
            GAS::type_tag(),
        )?;
        let balance = index_store.get_balance(address, GAS::type_tag())?;
        assert_eq!(balance, balance_from_db);
        assert_eq!(balance.balance, 1000);
        assert_eq!(balance.num_coins, 10);

        let all_balance = index_store.get_all_balance(address)?;
        let balance = all_balance.get(&GAS::type_tag()).unwrap();
        assert_eq!(*balance, balance_from_db);
        assert_eq!(balance.balance, 1000);
        assert_eq!(balance.num_coins, 10);

        written_objects.clear();
        let mut deleted_objects = vec![];
        for (id, object) in object_map.iter().take(3) {
            deleted_objects.push((address, *id));
            input_objects.insert(*id, object.to_owned());
        }
        let object_index_changes = ObjectIndexChanges {
            deleted_owners: deleted_objects.clone(),
            deleted_dynamic_fields: vec![],
            new_owners: vec![],
            new_dynamic_fields: vec![],
        };
        let tx_coins = (input_objects, written_objects);
        index_store.index_tx(
            address,
            vec![].into_iter(),
            vec![].into_iter(),
            vec![].into_iter(),
            &TransactionEvents { data: vec![] },
            object_index_changes,
            &TransactionDigest::random(),
            1234,
            Some(tx_coins),
        )?;
        let balance_from_db = IndexStore::get_balance_from_db(
            index_store.metrics.clone(),
            index_store.tables.coin_index_2.clone(),
            address,
            GAS::type_tag(),
        )?;
        let balance = index_store.get_balance(address, GAS::type_tag())?;
        assert_eq!(balance, balance_from_db);
        assert_eq!(balance.balance, 700);
        assert_eq!(balance.num_coins, 7);
        // Invalidate per coin type balance cache and read from all balance cache to ensure
        // the balance matches
        index_store
            .caches
            .per_coin_type_balance
            .invalidate(&(address, GAS::type_tag()));
        let all_balance = index_store.get_all_balance(address)?;
        assert_eq!(all_balance.get(&GAS::type_tag()).unwrap().balance, 700);
        assert_eq!(all_balance.get(&GAS::type_tag()).unwrap().num_coins, 7);
        let balance = index_store.get_balance(address, GAS::type_tag())?;
        assert_eq!(balance, balance_from_db);
        assert_eq!(balance.balance, 700);
        assert_eq!(balance.num_coins, 7);

        Ok(())
    }
}
