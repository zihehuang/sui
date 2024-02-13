// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! MemoryCache is a cache for the transaction execution which delays writes to the database until
//! transaction results are certified (i.e. they appear in a certified checkpoint, or an effects cert
//! is observed by a fullnode). The cache also stores committed data in memory in order to serve
//! future reads without hitting the database.
//!
//! For storing uncommitted transaction outputs, we cannot evict the data at all until it is written
//! to disk. Committed data not only can be evicted, but it is also unbounded (imagine a stream of
//! transactions that keep splitting a coin into smaller coins).
//!
//! We also want to be able to support negative cache hits (i.e. the case where we can determine an
//! object does not exist without hitting the database).
//!
//! To achieve both of these goals, we split the cache data into two pieces, a dirty set and a cached
//! set. The dirty set has no automatic evictions, data is only removed after being committed. The
//! cached set is in a bounded-sized cache with automatic evictions. In order to support negative
//! cache hits, we treat the two halves of the cache as FIFO queue. Newly written (dirty) versions are
//! inserted to one end of the dirty queue. As versions are committed to disk, they are
//! removed from the other end of the dirty queue and inserted into the cache queue. The cache queue
//! is truncated if it exceeds its maximum size, by removing the oldest versions.
//!
//! This gives us the property that the sequence of versions in the dirty and cached queues are the
//! most recent versions of the object, i.e. there can be no "gaps". This allows for the following:
//!
//!   - Negative cache hits: If the queried version is not in the cache, but is higher than the smallest
//!     version in the queue, it does not exist in the db either.
//!   - Bounded reads: When reading the most recent version that is <= some version bound, we can
//!     correctly satisfy this query from the cache, or determine that we must go to the db.
//!
//! Note that at any time, either or both the dirty or the cached queue may be non-existent. There may be no
//! dirty versions of the objects, in which case there will be no dirty queue. And, the cached queue
//! may be evicted from the cache, in which case there will be no cached queue. Because only the cached
//! queue can be evicted (the dirty queue can only become empty by moving versions from it to the cached
//! queue), the "highest versions" property still holds in all cases.
//!
//! The above design is used for both objects and markers.

use crate::authority::authority_per_epoch_store::AuthorityPerEpochStore;
use crate::authority::authority_store::{ExecutionLockWriteGuard, SuiLockResult};
use crate::authority::authority_store_pruner::{
    AuthorityStorePruner, AuthorityStorePruningMetrics,
};
use crate::authority::authority_store_tables::LiveObject;
use crate::authority::epoch_start_configuration::{EpochFlag, EpochStartConfiguration};
use crate::authority::AuthorityStore;
use crate::checkpoints::CheckpointStore;
use crate::state_accumulator::AccumulatorStore;
use crate::transaction_outputs::TransactionOutputs;

use dashmap::mapref::entry::Entry as DashMapEntry;
use dashmap::DashMap;
use either::Either;
use futures::{
    future::{join_all, BoxFuture},
    FutureExt,
};
use moka::sync::Cache as MokaCache;
use mysten_common::sync::notify_read::NotifyRead;
use parking_lot::Mutex;
use prometheus::Registry;
use std::collections::BTreeSet;
use std::sync::Arc;
use sui_config::node::AuthorityStorePruningConfig;
use sui_protocol_config::ProtocolVersion;
use sui_types::accumulator::Accumulator;
use sui_types::base_types::{EpochId, ObjectID, ObjectRef, SequenceNumber, VerifiedExecutionData};
use sui_types::digests::{
    ObjectDigest, TransactionDigest, TransactionEffectsDigest, TransactionEventsDigest,
};
use sui_types::effects::{TransactionEffects, TransactionEvents};
use sui_types::error::{SuiError, SuiResult, UserInputError};
use sui_types::message_envelope::Message;
use sui_types::messages_checkpoint::CheckpointSequenceNumber;
use sui_types::object::Object;
use sui_types::storage::{MarkerValue, ObjectKey, ObjectOrTombstone, ObjectStore, PackageObject};
use sui_types::sui_system_state::{get_sui_system_state, SuiSystemState};
use sui_types::transaction::VerifiedTransaction;
use tracing::instrument;

use super::{
    cached_version_map::CachedVersionMap, implement_passthrough_traits, CheckpointCache,
    ExecutionCacheCommit, ExecutionCacheMetrics, ExecutionCacheRead, ExecutionCacheReconfigAPI,
    ExecutionCacheWrite, NotifyReadWrapper, StateSyncAPI,
};

#[derive(Clone, PartialEq, Eq)]
enum ObjectEntry {
    Object(Object),
    Deleted,
    Wrapped,
}

impl std::fmt::Debug for ObjectEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectEntry::Object(o) => {
                write!(f, "ObjectEntry::Object({:?})", o.compute_object_reference())
            }
            ObjectEntry::Deleted => write!(f, "ObjectEntry::Deleted"),
            ObjectEntry::Wrapped => write!(f, "ObjectEntry::Wrapped"),
        }
    }
}

impl From<Object> for ObjectEntry {
    fn from(object: Object) -> Self {
        ObjectEntry::Object(object)
    }
}

type MarkerKey = (EpochId, ObjectID);

enum CacheResult<T> {
    /// Entry is in the cache
    Hit(T),
    /// Entry is not in the cache and is known to not exist
    NegativeHit,
    /// Entry is not in the cache and may or may not exist in the store
    Miss,
}

/// UncommitedData stores execution outputs that are not yet written to the db. Entries in this
/// struct can only be purged after they are committed.
struct UncommittedData {
    /// The object dirty set. All writes go into this table first. After we flush the data to the
    /// db, the data is removed from this table and inserted into the object_cache.
    ///
    /// This table may contain both live and dead objects, since we flush both live and dead
    /// objects to the db in order to support past object queries on fullnodes.
    /// When we move data into the object_cache we only retain the live objects.
    ///
    /// Further, we only remove objects in FIFO order, which ensures that the the cached
    /// sequence of objects has no gaps. In other words, if we have versions 4, 8, 13 of
    /// an object, we can deduce that version 9 does not exist. This also makes child object
    /// reads efficient. `object_cache` cannot contain a more recent version of an object than
    /// `objects`, and neither can have any gaps. Therefore if there is any object <= the version
    /// bound for a child read in objects, it is the correct object to return.
    objects: DashMap<ObjectID, CachedVersionMap<ObjectEntry>>,

    // Markers for received objects and deleted shared objects. This contains all of the dirty
    // marker state, which is committed to the db at the same time as other transaction data.
    // After markers are committed to the db we remove them from this table and insert them into
    // marker_cache.
    markers: DashMap<MarkerKey, CachedVersionMap<MarkerValue>>,

    transaction_effects: DashMap<TransactionEffectsDigest, TransactionEffects>,

    // Because TransactionEvents are not unique to the transaction that created them, we must
    // reference count them in order to know when we can remove them from the cache. For now
    // we track all referers explicitlyl, but we can use a ref count when we are confident in
    // the correctness of the code.
    transaction_events:
        DashMap<TransactionEventsDigest, (BTreeSet<TransactionDigest>, TransactionEvents)>,

    executed_effects_digests: DashMap<TransactionDigest, TransactionEffectsDigest>,

    // Transaction outputs that have not yet been written to the DB. Items are removed from this
    // table as they are flushed to the db.
    pending_transaction_writes: DashMap<TransactionDigest, Arc<TransactionOutputs>>,
}

impl UncommittedData {
    fn new() -> Self {
        Self {
            objects: DashMap::new(),
            markers: DashMap::new(),
            transaction_effects: DashMap::new(),
            executed_effects_digests: DashMap::new(),
            pending_transaction_writes: DashMap::new(),
            transaction_events: DashMap::new(),
        }
    }
}

/// CachedData stores data that has been committed to the db, but is likely to be read soon.
struct CachedData {
    /// Contains live, non-package objects that have been committed to the db.
    /// As with `objects`, we remove objects from this table in FIFO order (or we allow the cache
    /// to evict all versions of the object at once), which ensures that the the cached sequence
    /// of objects has no gaps. See the comment above for more details.
    // TODO(cache): this is not populated yet, we will populate it when we implement flushing.
    object_cache: MokaCache<ObjectID, Arc<Mutex<CachedVersionMap<ObjectEntry>>>>,

    // Packages are cached separately from objects because they are immutable and can be used by any
    // number of transactions. Additionally, many operations require loading large numbers of packages
    // (due to dependencies), so we want to try to keep all packages in memory.
    // Note that, like any other dirty object, all packages are also stored in `objects` until they are
    // flushed to disk.
    packages: MokaCache<ObjectID, PackageObject>,

    // Because markers (e.g. received markers) can be read by many transactions, we also cache
    // them. Markers are added to this cache in two ways:
    // 1. When they are committed to the db and removed from the `markers` table.
    // 2. After a cache miss in which we retrieve the marker from the db.

    // Note that MokaCache can only return items by value, so we store the map as an Arc<Mutex>.
    // (There should be no contention on the inner mutex, it is used only for interior mutability.)
    marker_cache: MokaCache<MarkerKey, Arc<Mutex<CachedVersionMap<MarkerValue>>>>,

    // Objects that were read at transaction signing time - allows us to access them again at
    // execution time with a single lock / hash lookup
    _transaction_objects: MokaCache<TransactionDigest, Vec<Object>>,
}

impl CachedData {
    fn new() -> Self {
        let object_cache = MokaCache::builder()
            .max_capacity(10000)
            .initial_capacity(10000)
            .build();
        let packages = MokaCache::builder()
            .max_capacity(10000)
            .initial_capacity(10000)
            .build();
        let marker_cache = MokaCache::builder()
            .max_capacity(10000)
            .initial_capacity(10000)
            .build();
        let transaction_objects = MokaCache::builder()
            .max_capacity(10000)
            .initial_capacity(10000)
            .build();

        Self {
            object_cache,
            packages,
            marker_cache,
            _transaction_objects: transaction_objects,
        }
    }
}
pub struct MemoryCache {
    dirty: UncommittedData,
    cached: CachedData,

    executed_effects_digests_notify_read: NotifyRead<TransactionDigest, TransactionEffectsDigest>,
    store: Arc<AuthorityStore>,
    metrics: Option<ExecutionCacheMetrics>,
}

impl MemoryCache {
    pub fn new(store: Arc<AuthorityStore>, registry: &Registry) -> Self {
        Self {
            dirty: UncommittedData::new(),
            cached: CachedData::new(),
            executed_effects_digests_notify_read: NotifyRead::new(),
            store,
            metrics: Some(ExecutionCacheMetrics::new(registry)),
        }
    }

    pub fn new_with_no_metrics(store: Arc<AuthorityStore>) -> Self {
        Self {
            dirty: UncommittedData::new(),
            cached: CachedData::new(),
            executed_effects_digests_notify_read: NotifyRead::new(),
            store,
            metrics: None,
        }
    }

    // Insert a new object in the dirty state. The object will not be persisted to disk.
    fn write_object(&self, object_id: &ObjectID, object: &Object) {
        let version = object.version();
        tracing::debug!("inserting object {:?}: {:?}", object_id, version);
        self.dirty
            .objects
            .entry(*object_id)
            .or_default()
            .insert(object.version(), object.clone().into());
    }

    // Insert a deleted tombstone in the dirty state. The tombstone will not be persisted to disk.
    fn write_deleted_tombstone(&self, object_id: &ObjectID, version: SequenceNumber) {
        tracing::debug!("inserting deleted tombstone {:?}: {:?}", object_id, version);
        self.dirty
            .objects
            .entry(*object_id)
            .or_default()
            .insert(version, ObjectEntry::Deleted);
    }

    // Insert a wrapped tombstone in the dirty state. The tombstone will not be persisted to disk.
    fn write_wrapped_tombstone(&self, object_id: &ObjectID, version: SequenceNumber) {
        tracing::debug!("inserting wrapped tombstone {:?}: {:?}", object_id, version);
        self.dirty
            .objects
            .entry(*object_id)
            .or_default()
            .insert(version, ObjectEntry::Wrapped);
    }

    // Attempt to get an object from the cache. The DB is not consulted.
    // Can return Hit, Miss, or NegativeHit (if the object is known to not exist).
    fn get_object_entry_by_key_cache_only(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> CacheResult<ObjectEntry> {
        macro_rules! check_cache_entry {
            ($objects: expr) => {
                if let Some(entry) = $objects.get(&version) {
                    return CacheResult::Hit(entry.clone());
                }

                if let Some(last_version) = $objects.get_last() {
                    if last_version.0 < version {
                        // If the version is greater than the last version in the cache, then we know
                        // that the object does not exist anywhere
                        return CacheResult::NegativeHit;
                    }
                }
            };
        }

        if let Some(objects) = self.dirty.objects.get(object_id) {
            check_cache_entry!(objects);
        }

        if let Some(objects) = self.cached.object_cache.get(object_id) {
            let objects = objects.lock();
            check_cache_entry!(objects);
        }

        CacheResult::Miss
    }

    fn get_object_by_key_cache_only(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> CacheResult<Object> {
        match self.get_object_entry_by_key_cache_only(object_id, version) {
            CacheResult::Hit(entry) => match entry {
                ObjectEntry::Object(object) => CacheResult::Hit(object),
                ObjectEntry::Deleted | ObjectEntry::Wrapped => CacheResult::NegativeHit,
            },
            CacheResult::Miss => CacheResult::Miss,
            CacheResult::NegativeHit => CacheResult::NegativeHit,
        }
    }

    fn get_object_entry_by_id_cache_only(
        &self,
        object_id: &ObjectID,
    ) -> CacheResult<(SequenceNumber, ObjectEntry)> {
        macro_rules! check_cache_entry {
            ($objects: expr) => {
                if let Some((version, entry)) = $objects.get_last() {
                    return CacheResult::Hit((*version, entry.clone()));
                } else {
                    return CacheResult::Miss;
                }
            };
        }

        if let Some(objects) = self.dirty.objects.get(object_id) {
            check_cache_entry!(objects);
        }

        if let Some(objects) = self.cached.object_cache.get(object_id) {
            let objects = objects.lock();
            check_cache_entry!(objects);
        }

        CacheResult::Miss
    }

    fn get_object_by_id_cache_only(
        &self,
        object_id: &ObjectID,
    ) -> CacheResult<(SequenceNumber, Object)> {
        match self.get_object_entry_by_id_cache_only(object_id) {
            CacheResult::Hit((version, entry)) => match entry {
                ObjectEntry::Object(object) => CacheResult::Hit((version, object)),
                ObjectEntry::Deleted | ObjectEntry::Wrapped => CacheResult::NegativeHit,
            },
            CacheResult::NegativeHit => CacheResult::NegativeHit,
            CacheResult::Miss => CacheResult::Miss,
        }
    }

    // Commits dirty data for the given TransactionDigest to the db.
    async fn commit_transaction_outputs(
        &self,
        epoch: EpochId,
        digest: TransactionDigest,
    ) -> SuiResult {
        let Some((_, outputs)) = self.dirty.pending_transaction_writes.remove(&digest) else {
            return Err(SuiError::TransactionNotFound { digest });
        };

        // Flush writes to disk
        self.store
            .write_transaction_outputs(epoch, outputs.clone())
            .await?;

        // Now, remove each piece of committed data from the dirty state and insert it into the cache.
        // TODO: outputs should have a strong count of 1 so we should be able to move out of it
        let TransactionOutputs {
            transaction,
            effects,
            markers,
            written,
            deleted,
            wrapped,
            events,
            ..
        } = &*outputs;

        // Move dirty markers to cache
        for (object_key, marker_value) in markers.iter() {
            Self::move_version_from_dirty_to_cache(
                &self.dirty.markers,
                &self.cached.marker_cache,
                (epoch, object_key.0),
                object_key.1,
                marker_value,
            );
        }

        for (object_id, object) in written.iter() {
            Self::move_version_from_dirty_to_cache(
                &self.dirty.objects,
                &self.cached.object_cache,
                *object_id,
                object.version(),
                &ObjectEntry::Object(object.clone()),
            );
        }

        for ObjectKey(object_id, version) in deleted.iter() {
            Self::move_version_from_dirty_to_cache(
                &self.dirty.objects,
                &self.cached.object_cache,
                *object_id,
                *version,
                &ObjectEntry::Deleted,
            );
        }

        for ObjectKey(object_id, version) in wrapped.iter() {
            Self::move_version_from_dirty_to_cache(
                &self.dirty.objects,
                &self.cached.object_cache,
                *object_id,
                *version,
                &ObjectEntry::Wrapped,
            );
        }

        let tx_digest = *transaction.digest();
        let effects_digest = effects.digest();

        self.dirty
            .transaction_effects
            .remove(&effects_digest)
            .expect("effects must exist");

        match self.dirty.transaction_events.entry(events.digest()) {
            DashMapEntry::Occupied(mut occupied) => {
                let txns = &mut occupied.get_mut().0;
                assert!(txns.remove(&tx_digest), "transaction must exist");
                if txns.is_empty() {
                    occupied.remove();
                }
            }
            DashMapEntry::Vacant(_) => {
                panic!("events must exist");
            }
        }

        self.dirty
            .executed_effects_digests
            .remove(&tx_digest)
            .expect("executed effects must exist");

        Ok(())
    }

    // Move an entry from the dirty queue to the cache queue. This is called after the entry is
    // committed to the db.
    fn move_version_from_dirty_to_cache<K, V>(
        dirty: &DashMap<K, CachedVersionMap<V>>,
        cache: &MokaCache<K, Arc<Mutex<CachedVersionMap<V>>>>,
        key: K,
        version: SequenceNumber,
        value: &V,
    ) where
        K: Eq + std::hash::Hash + Clone + Send + Sync + Copy + 'static,
        V: Send + Sync + Clone + Eq + std::fmt::Debug + 'static,
    {
        static MAX_VERSIONS: usize = 3;

        // IMPORTANT: lock both the dirty set entry and the cache entry before modifying either.
        // this ensures that readers cannot see a value temporarily disappear.
        let cache_entry = cache.entry(key).or_default();
        let mut cache_map = cache_entry.value().lock();
        let dirty_entry = dirty.entry(key);

        // insert into cache and drop old versions.
        cache_map.insert(version, value.clone());
        // TODO: make this automatic by giving CachedVersionMap an optional max capacity
        cache_map.truncate(MAX_VERSIONS);

        let DashMapEntry::Occupied(mut occupied_dirty_entry) = dirty_entry else {
            panic!("dirty map must exist");
        };

        let removed = occupied_dirty_entry.get_mut().remove(&version);

        assert_eq!(removed.as_ref(), Some(value), "dirty version must exist");

        // if there are no versions remaining, remove the map entry
        if occupied_dirty_entry.get().is_empty() {
            occupied_dirty_entry.remove();
        }
    }

    pub async fn prune_objects_and_compact_for_testing(
        &self,
        checkpoint_store: &Arc<CheckpointStore>,
    ) {
        let pruning_config = AuthorityStorePruningConfig {
            num_epochs_to_retain: 0,
            ..Default::default()
        };
        let _ = AuthorityStorePruner::prune_objects_for_eligible_epochs(
            &self.store.perpetual_tables,
            checkpoint_store,
            &self.store.objects_lock_table,
            pruning_config,
            AuthorityStorePruningMetrics::new_for_test(),
            usize::MAX,
        )
        .await;
        let _ = AuthorityStorePruner::compact(&self.store.perpetual_tables);
    }

    pub fn store_for_testing(&self) -> &Arc<AuthorityStore> {
        &self.store
    }

    pub fn as_notify_read_wrapper(self: Arc<Self>) -> NotifyReadWrapper<Self> {
        NotifyReadWrapper(self)
    }
}

impl ExecutionCacheCommit for MemoryCache {
    fn commit_transaction_outputs(
        &self,
        epoch: EpochId,
        digest: &TransactionDigest,
    ) -> BoxFuture<'_, SuiResult> {
        MemoryCache::commit_transaction_outputs(self, epoch, *digest).boxed()
    }
}

impl ExecutionCacheRead for MemoryCache {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        if let Some(p) = self.cached.packages.get(package_id) {
            #[cfg(debug_assertions)]
            {
                if let Some(store_package) = self.store.get_object(package_id).unwrap() {
                    assert_eq!(
                        store_package.digest(),
                        p.object().digest(),
                        "Package object cache is inconsistent for package {:?}",
                        package_id
                    );
                }
            }
            return Ok(Some(p));
        }

        // We try the dirty objects cache as well before going to the database. This is necessary
        // because the package could be evicted from the package cache before it is committed
        // to the database.
        if let Some(p) = ExecutionCacheRead::get_object(self, package_id)? {
            if p.is_package() {
                let p = PackageObject::new(p);
                self.cached.packages.insert(*package_id, p.clone());
                Ok(Some(p))
            } else {
                Err(SuiError::UserInputError {
                    error: UserInputError::MoveObjectAsPackage {
                        object_id: *package_id,
                    },
                })
            }
        } else {
            Ok(None)
        }
    }

    fn force_reload_system_packages(&self, system_package_ids: &[ObjectID]) {
        for package_id in system_package_ids {
            if let Some(p) = self
                .store
                .get_object(package_id)
                .expect("Failed to update system packages")
            {
                assert!(p.is_package());
                self.cached
                    .packages
                    .insert(*package_id, PackageObject::new(p));
            }
            // It's possible that a package is not found if it's newly added system package ID
            // that hasn't got created yet. This should be very very rare though.
        }
    }

    // get_object and variants.
    //
    // TODO: We don't insert objects into the cache after misses because they are usually only
    // read once. We might want to cache immutable reads (RO shared objects and immutable objects)
    // If we do this, we must be VERY CAREFUL not to break the contiguous version property
    // of the cache.

    fn get_object(&self, id: &ObjectID) -> SuiResult<Option<Object>> {
        match self.get_object_by_id_cache_only(id) {
            CacheResult::Hit((_, object)) => Ok(Some(object)),
            CacheResult::NegativeHit => Ok(None),
            CacheResult::Miss => Ok(self.store.get_object(id)?),
        }
    }

    fn get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        match self.get_object_by_key_cache_only(object_id, version) {
            CacheResult::Hit(object) => Ok(Some(object)),
            CacheResult::NegativeHit => Ok(None),
            CacheResult::Miss => Ok(self.store.get_object_by_key(object_id, version)?),
        }
    }

    fn multi_get_objects_by_key(
        &self,
        object_keys: &[ObjectKey],
    ) -> Result<Vec<Option<Object>>, SuiError> {
        let mut results = vec![None; object_keys.len()];
        let mut fallback_keys = Vec::with_capacity(object_keys.len());
        let mut fetch_indices = Vec::with_capacity(object_keys.len());

        for (i, key) in object_keys.iter().enumerate() {
            match self.get_object_by_key_cache_only(&key.0, key.1) {
                CacheResult::Hit(object) => results[i] = Some(object),
                CacheResult::NegativeHit => (),
                CacheResult::Miss => {
                    fallback_keys.push(*key);
                    fetch_indices.push(i);
                }
            }
        }

        let store_results = self.store.multi_get_objects_by_key(&fallback_keys)?;
        assert_eq!(store_results.len(), fetch_indices.len());
        assert_eq!(store_results.len(), fallback_keys.len());

        for (i, result) in fetch_indices.into_iter().zip(store_results.into_iter()) {
            results[i] = result;
        }

        Ok(results)
    }

    fn object_exists_by_key(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> SuiResult<bool> {
        match self.get_object_by_key_cache_only(object_id, version) {
            CacheResult::Hit(_) => Ok(true),
            CacheResult::NegativeHit => Ok(false),
            CacheResult::Miss => self.store.object_exists_by_key(object_id, version),
        }
    }

    fn multi_object_exists_by_key(&self, object_keys: &[ObjectKey]) -> SuiResult<Vec<bool>> {
        do_fallback_lookup(
            object_keys,
            |key| match self.get_object_by_key_cache_only(&key.0, key.1) {
                CacheResult::Hit(_) => CacheResult::Hit(true),
                CacheResult::NegativeHit => CacheResult::Hit(false),
                CacheResult::Miss => CacheResult::Miss,
            },
            |remaining| self.store.multi_object_exists_by_key(remaining),
        )
    }

    fn get_latest_object_ref_or_tombstone(
        &self,
        object_id: ObjectID,
    ) -> SuiResult<Option<ObjectRef>> {
        match self.get_object_entry_by_id_cache_only(&object_id) {
            CacheResult::Hit((version, entry)) => Ok(Some(match entry {
                ObjectEntry::Object(object) => object.compute_object_reference(),
                ObjectEntry::Deleted => (object_id, version, ObjectDigest::OBJECT_DIGEST_DELETED),
                ObjectEntry::Wrapped => (object_id, version, ObjectDigest::OBJECT_DIGEST_WRAPPED),
            })),
            CacheResult::NegativeHit => Ok(None),
            CacheResult::Miss => self.store.get_latest_object_ref_or_tombstone(object_id),
        }
    }

    fn get_latest_object_or_tombstone(
        &self,
        object_id: ObjectID,
    ) -> Result<Option<(ObjectKey, ObjectOrTombstone)>, SuiError> {
        match self.get_object_entry_by_id_cache_only(&object_id) {
            CacheResult::Hit((version, entry)) => {
                let key = ObjectKey(object_id, version);
                Ok(Some(match entry {
                    ObjectEntry::Object(object) => (key, object.into()),
                    ObjectEntry::Deleted => (
                        key,
                        ObjectOrTombstone::Tombstone((
                            object_id,
                            version,
                            ObjectDigest::OBJECT_DIGEST_DELETED,
                        )),
                    ),
                    ObjectEntry::Wrapped => (
                        key,
                        ObjectOrTombstone::Tombstone((
                            object_id,
                            version,
                            ObjectDigest::OBJECT_DIGEST_WRAPPED,
                        )),
                    ),
                }))
            }
            CacheResult::NegativeHit => Ok(None),
            CacheResult::Miss => self.store.get_latest_object_or_tombstone(object_id),
        }
    }

    fn find_object_lt_or_eq_version(
        &self,
        object_id: ObjectID,
        version: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        macro_rules! check_cache_entry {
            ($objects: expr) => {
                if let Some((_, object)) = $objects.all_lt_or_eq_rev(&version).next() {
                    if let ObjectEntry::Object(object) = object {
                        return Ok(Some(object.clone()));
                    } else {
                        // if we find a tombstone, the object does not exist
                        return Ok(None);
                    }
                }
            };
        }

        if let Some(objects) = self.dirty.objects.get(&object_id) {
            check_cache_entry!(objects);
        }

        if let Some(objects) = self.cached.object_cache.get(&object_id) {
            let objects = objects.lock();
            check_cache_entry!(objects);
        }

        self.store.find_object_lt_or_eq_version(object_id, version)
    }

    fn multi_get_transaction_blocks(
        &self,
        digests: &[TransactionDigest],
    ) -> SuiResult<Vec<Option<Arc<VerifiedTransaction>>>> {
        do_fallback_lookup(
            digests,
            |digest| {
                if let Some(tx) = self.dirty.pending_transaction_writes.get(digest) {
                    CacheResult::Hit(Some(tx.transaction.clone()))
                } else {
                    CacheResult::Miss
                }
            },
            |remaining| {
                self.store
                    .multi_get_transaction_blocks(remaining)
                    .map(|v| v.into_iter().map(|o| o.map(Arc::new)).collect())
            },
        )
    }

    fn multi_get_executed_effects_digests(
        &self,
        digests: &[TransactionDigest],
    ) -> SuiResult<Vec<Option<TransactionEffectsDigest>>> {
        do_fallback_lookup(
            digests,
            |digest| {
                if let Some(digest) = self.dirty.executed_effects_digests.get(digest) {
                    CacheResult::Hit(Some(*digest))
                } else {
                    CacheResult::Miss
                }
            },
            |remaining| self.store.multi_get_executed_effects_digests(remaining),
        )
    }

    fn multi_get_effects(
        &self,
        digests: &[TransactionEffectsDigest],
    ) -> SuiResult<Vec<Option<TransactionEffects>>> {
        do_fallback_lookup(
            digests,
            |digest| {
                if let Some(effects) = self.dirty.transaction_effects.get(digest) {
                    CacheResult::Hit(Some(effects.clone()))
                } else {
                    CacheResult::Miss
                }
            },
            |remaining| self.store.multi_get_effects(remaining.iter()),
        )
    }

    fn notify_read_executed_effects_digests<'a>(
        &'a self,
        digests: &'a [TransactionDigest],
    ) -> BoxFuture<'a, SuiResult<Vec<TransactionEffectsDigest>>> {
        async move {
            let registrations = self
                .executed_effects_digests_notify_read
                .register_all(digests);

            let executed_effects_digests = self.multi_get_executed_effects_digests(digests)?;

            let results = executed_effects_digests
                .into_iter()
                .zip(registrations)
                .map(|(a, r)| match a {
                    // Note that Some() clause also drops registration that is already fulfilled
                    Some(ready) => Either::Left(futures::future::ready(ready)),
                    None => Either::Right(r),
                });

            Ok(join_all(results).await)
        }
        .boxed()
    }

    fn multi_get_events(
        &self,
        event_digests: &[TransactionEventsDigest],
    ) -> SuiResult<Vec<Option<TransactionEvents>>> {
        do_fallback_lookup(
            event_digests,
            |digest| {
                if let Some(events) = self.dirty.transaction_events.get(digest) {
                    CacheResult::Hit(Some(events.1.clone()))
                } else {
                    CacheResult::Miss
                }
            },
            |digests| self.store.multi_get_events(digests),
        )
    }

    fn get_sui_system_state_object_unsafe(&self) -> SuiResult<SuiSystemState> {
        get_sui_system_state(self)
    }

    fn get_marker_value(
        &self,
        object_id: &ObjectID,
        version: &SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<MarkerValue>> {
        if let Some(markers) = self.dirty.markers.get(&(epoch_id, *object_id)) {
            if let Some(marker) = markers.get(version) {
                return Ok(Some(*marker));
            }
        }

        if let Some(markers) = self.cached.marker_cache.get(&(epoch_id, *object_id)) {
            if let Some(marker) = markers.lock().get(version) {
                return Ok(Some(*marker));
            }
        }

        // NOTE: we cannot insert this marker into the cache without breaking the
        // contiguous version property of the cache.
        self.store.get_marker_value(object_id, version, epoch_id)
    }

    fn get_latest_marker(
        &self,
        object_id: &ObjectID,
        epoch_id: EpochId,
    ) -> SuiResult<Option<(SequenceNumber, MarkerValue)>> {
        if let Some(markers) = self.dirty.markers.get(&(epoch_id, *object_id)) {
            if let Some((k, v)) = markers.get_last() {
                return Ok(Some((*k, *v)));
            }
        }

        if let Some(markers) = self.cached.marker_cache.get(&(epoch_id, *object_id)) {
            let markers = markers.lock();
            if let Some((k, v)) = markers.get_last() {
                return Ok(Some((*k, *v)));
            }
        }

        // TODO: we could safely insert this marker into the cache because it is the latest
        self.store.get_latest_marker(object_id, epoch_id)
    }

    fn get_lock(&self, _obj_ref: ObjectRef, _epoch_id: EpochId) -> SuiLockResult {
        todo!()
    }

    fn get_latest_lock_for_object_id(&self, _object_id: ObjectID) -> SuiResult<ObjectRef> {
        todo!()
    }

    fn check_owned_object_locks_exist(&self, _owned_object_refs: &[ObjectRef]) -> SuiResult {
        todo!()
    }
}

impl ExecutionCacheWrite for MemoryCache {
    #[instrument(level = "trace", skip_all)]
    fn acquire_transaction_locks<'a>(
        &'a self,
        _epoch_id: EpochId,
        _owned_input_objects: &'a [ObjectRef],
        _tx_digest: TransactionDigest,
    ) -> BoxFuture<'a, SuiResult> {
        todo!()
    }

    #[instrument(level = "debug", skip_all)]
    fn write_transaction_outputs(
        &self,
        epoch_id: EpochId,
        tx_outputs: Arc<TransactionOutputs>,
    ) -> BoxFuture<'_, SuiResult> {
        let TransactionOutputs {
            transaction,
            effects,
            markers,
            written,
            deleted,
            wrapped,
            events,
            ..
        } = &*tx_outputs;

        // Update all markers
        for (object_key, marker_value) in markers.iter() {
            self.dirty
                .markers
                .entry((epoch_id, object_key.0))
                .or_default()
                .value_mut()
                .insert(object_key.1, *marker_value);
        }

        // Write children before parents to ensure that readers do not observe a parent object
        // before its most recent children are visible.
        for (object_id, object) in written.iter() {
            if object.is_child_object() {
                self.write_object(object_id, object);
            }
        }
        for (object_id, object) in written.iter() {
            if !object.is_child_object() {
                self.write_object(object_id, object);
                if object.is_package() {
                    self.cached
                        .packages
                        .insert(*object_id, PackageObject::new(object.clone()));
                }
            }
        }

        for ObjectKey(id, version) in deleted.iter() {
            self.write_deleted_tombstone(id, *version);
        }
        for ObjectKey(id, version) in wrapped.iter() {
            self.write_wrapped_tombstone(id, *version);
        }

        let tx_digest = *transaction.digest();
        let effects_digest = effects.digest();

        self.dirty
            .transaction_effects
            .insert(effects_digest, effects.clone());

        match self.dirty.transaction_events.entry(events.digest()) {
            DashMapEntry::Occupied(mut occupied) => {
                occupied.get_mut().0.insert(tx_digest);
            }
            DashMapEntry::Vacant(entry) => {
                let mut txns = BTreeSet::new();
                txns.insert(tx_digest);
                entry.insert((txns, events.clone()));
            }
        }

        self.dirty
            .executed_effects_digests
            .insert(tx_digest, effects_digest);

        self.dirty
            .pending_transaction_writes
            .insert(tx_digest, tx_outputs);

        self.executed_effects_digests_notify_read
            .notify(&tx_digest, &effects_digest);

        if let Some(metrics) = &self.metrics {
            metrics
                .pending_notify_read
                .set(self.executed_effects_digests_notify_read.num_pending() as i64);
        }

        std::future::ready(Ok(())).boxed()
    }
}

fn do_fallback_lookup<K: Copy, V: Default + Clone>(
    keys: &[K],
    get_cached_key: impl Fn(&K) -> CacheResult<V>,
    multiget_fallback: impl Fn(&[K]) -> SuiResult<Vec<V>>,
) -> SuiResult<Vec<V>> {
    let mut results = vec![V::default(); keys.len()];
    let mut fallback_keys = Vec::with_capacity(keys.len());
    let mut fallback_indices = Vec::with_capacity(keys.len());

    for (i, key) in keys.iter().enumerate() {
        match get_cached_key(key) {
            CacheResult::Miss => {
                fallback_keys.push(*key);
                fallback_indices.push(i);
            }
            CacheResult::NegativeHit => (),
            CacheResult::Hit(value) => {
                results[i] = value;
            }
        }
    }

    let fallback_results = multiget_fallback(&fallback_keys)?;
    assert_eq!(fallback_results.len(), fallback_indices.len());
    assert_eq!(fallback_results.len(), fallback_keys.len());

    for (i, result) in fallback_indices
        .into_iter()
        .zip(fallback_results.into_iter())
    {
        results[i] = result;
    }
    Ok(results)
}

implement_passthrough_traits!(MemoryCache);

impl AccumulatorStore for MemoryCache {
    fn get_object_ref_prior_to_key_deprecated(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
    ) -> SuiResult<Option<ObjectRef>> {
        // There is probably a more efficient way to implement this, but since this is only used by
        // old protocol versions, it is better to do the simple thing that is obviously correct.
        // In this case we previous version from all sources and choose the highest
        let mut candidates = Vec::new();

        let check_versions =
            |versions: &CachedVersionMap<ObjectEntry>| match versions.get_prior_to(&version) {
                Some((version, object_entry)) => match object_entry {
                    ObjectEntry::Object(object) => {
                        assert_eq!(object.version(), version);
                        Some(object.compute_object_reference())
                    }
                    ObjectEntry::Deleted => {
                        Some((*object_id, version, ObjectDigest::OBJECT_DIGEST_DELETED))
                    }
                    ObjectEntry::Wrapped => {
                        Some((*object_id, version, ObjectDigest::OBJECT_DIGEST_WRAPPED))
                    }
                },
                None => None,
            };

        // first check dirty data
        if let Some(objects) = self.dirty.objects.get(object_id) {
            if let Some(prior) = check_versions(&objects) {
                candidates.push(prior);
            }
        }

        if let Some(objects) = self.cached.object_cache.get(object_id) {
            if let Some(prior) = check_versions(&objects.lock()) {
                candidates.push(prior);
            }
        }

        if let Some(prior) = self
            .store
            .get_object_ref_prior_to_key_deprecated(object_id, version)?
        {
            candidates.push(prior);
        }

        // sort candidates by version, and return the highest
        candidates.sort_by_key(|(_, version, _)| *version);
        Ok(candidates.pop())
    }

    fn get_root_state_accumulator_for_epoch(
        &self,
        epoch: EpochId,
    ) -> SuiResult<Option<(CheckpointSequenceNumber, Accumulator)>> {
        self.store.get_root_state_accumulator_for_epoch(epoch)
    }

    fn get_root_state_accumulator_for_highest_epoch(
        &self,
    ) -> SuiResult<Option<(EpochId, (CheckpointSequenceNumber, Accumulator))>> {
        self.store.get_root_state_accumulator_for_highest_epoch()
    }

    fn insert_state_accumulator_for_epoch(
        &self,
        epoch: EpochId,
        checkpoint_seq_num: &CheckpointSequenceNumber,
        acc: &Accumulator,
    ) -> SuiResult {
        self.store
            .insert_state_accumulator_for_epoch(epoch, checkpoint_seq_num, acc)
    }

    fn iter_live_object_set(
        &self,
        include_wrapped_tombstone: bool,
    ) -> Box<dyn Iterator<Item = LiveObject> + '_> {
        self.store.iter_live_object_set(include_wrapped_tombstone)
    }
}