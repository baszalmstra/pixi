//! Streaming decorator for [`rattler::install::Reporter`].
//!
//! Wraps an inner reporter and forwards every call. Each `on_*_start`
//! call additionally allocates a fresh [`OperationId`] (parented to the
//! parent install via the registry's task-local), records the
//! mapping from rattler's returned `usize` index to the new
//! [`OperationId`], and emits the matching [`Event`] variant. The
//! matching `_complete` call looks the id up and emits the
//! corresponding completion event.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use pixi_compute_reporters::{OperationId, OperationRegistry};
use rattler::install::{Reporter as RattlerInstallReporter, Transaction};
use rattler_conda_types::{PrefixRecord, RepoDataRecord};

use crate::events::{
    Event, EventEnvelope, PackageRef, TransactionOpKind, TransactionOpSummary,
    sink::SinkHandle,
};

/// One map per sub-step kind. Keys are the indices rattler returned
/// from the matching `_start` call; values are the OperationIds we
/// allocated and emitted events with. The `cache_entries` map also
/// remembers the [`PackageRef`] from `on_populate_cache_start` so the
/// downstream validate/download events can carry it (rattler's API
/// only hands those calls a cache-entry index, not the record).
#[derive(Default)]
struct IndexMaps {
    operations: HashMap<usize, OperationId>,
    cache_entries: HashMap<usize, (OperationId, PackageRef)>,
    validates: HashMap<usize, OperationId>,
    downloads: HashMap<usize, OperationId>,
    links: HashMap<usize, OperationId>,
    unlinks: HashMap<usize, OperationId>,
    post_links: HashMap<usize, OperationId>,
    pre_unlinks: HashMap<usize, OperationId>,
}

pub struct StreamingRattlerInstallReporter {
    inner: Box<dyn RattlerInstallReporter>,
    sink: SinkHandle,
    registry: Arc<OperationRegistry>,
    install_id: OperationId,
    maps: Mutex<IndexMaps>,
}

impl StreamingRattlerInstallReporter {
    pub fn new(
        inner: Box<dyn RattlerInstallReporter>,
        sink: SinkHandle,
        registry: Arc<OperationRegistry>,
        install_id: OperationId,
    ) -> Self {
        Self {
            inner,
            sink,
            registry,
            install_id,
            maps: Mutex::new(IndexMaps::default()),
        }
    }

    fn emit(&self, event: Event) {
        self.sink.record(EventEnvelope::new(event));
    }

    fn alloc(&self) -> OperationId {
        self.registry.allocate()
    }
}

impl RattlerInstallReporter for StreamingRattlerInstallReporter {
    fn on_transaction_start(&self, transaction: &Transaction<PrefixRecord, RepoDataRecord>) {
        let operations: Vec<TransactionOpSummary> = transaction
            .operations
            .iter()
            .map(transaction_op_summary)
            .collect();
        self.emit(Event::InstallTransactionStarted {
            id: self.install_id,
            operations,
        });
        self.inner.on_transaction_start(transaction);
    }

    fn on_transaction_operation_start(&self, operation: usize) {
        let id = self.alloc();
        let op = transaction_op_index_summary(operation);
        self.maps.lock().unwrap().operations.insert(operation, id);
        self.emit(Event::InstallOperationStarted {
            id,
            parent: self.install_id,
            op_idx: operation,
            op,
        });
        self.inner.on_transaction_operation_start(operation);
    }

    fn on_populate_cache_start(&self, operation: usize, record: &RepoDataRecord) -> usize {
        let inner_idx = self.inner.on_populate_cache_start(operation, record);
        let id = self.alloc();
        let package = package_from_repodata(record);
        let parent = self
            .maps
            .lock()
            .unwrap()
            .operations
            .get(&operation)
            .copied()
            .unwrap_or(self.install_id);
        self.maps
            .lock()
            .unwrap()
            .cache_entries
            .insert(inner_idx, (id, package.clone()));
        self.emit(Event::InstallCachePopulateStarted {
            id,
            parent,
            package,
        });
        inner_idx
    }

    fn on_validate_start(&self, cache_entry: usize) -> usize {
        let inner_idx = self.inner.on_validate_start(cache_entry);
        let id = self.alloc();
        let (parent, package) = self
            .maps
            .lock()
            .unwrap()
            .cache_entries
            .get(&cache_entry)
            .cloned()
            .unwrap_or((self.install_id, PackageRef::unknown()));
        self.maps.lock().unwrap().validates.insert(inner_idx, id);
        self.emit(Event::InstallValidateStarted {
            id,
            parent,
            package,
        });
        inner_idx
    }

    fn on_validate_complete(&self, validate_idx: usize) {
        if let Some(id) = self.maps.lock().unwrap().validates.remove(&validate_idx) {
            self.emit(Event::InstallValidateComplete { id });
        }
        self.inner.on_validate_complete(validate_idx);
    }

    fn on_download_start(&self, cache_entry: usize) -> usize {
        let inner_idx = self.inner.on_download_start(cache_entry);
        let id = self.alloc();
        let (parent, package) = self
            .maps
            .lock()
            .unwrap()
            .cache_entries
            .get(&cache_entry)
            .cloned()
            .unwrap_or((self.install_id, PackageRef::unknown()));
        let size_bytes = package.size_bytes;
        self.maps.lock().unwrap().downloads.insert(inner_idx, id);
        self.emit(Event::InstallDownloadStarted {
            id,
            parent,
            package,
            size_bytes,
        });
        inner_idx
    }

    fn on_download_progress(&self, download_idx: usize, progress: u64, total: Option<u64>) {
        if let Some(&id) = self.maps.lock().unwrap().downloads.get(&download_idx) {
            self.emit(Event::InstallDownloadProgress {
                id,
                bytes: progress,
                total,
            });
        }
        self.inner
            .on_download_progress(download_idx, progress, total);
    }

    fn on_download_completed(&self, download_idx: usize) {
        if let Some(id) = self.maps.lock().unwrap().downloads.remove(&download_idx) {
            self.emit(Event::InstallDownloadComplete { id });
        }
        self.inner.on_download_completed(download_idx);
    }

    fn on_populate_cache_complete(&self, cache_entry: usize) {
        if let Some((id, _)) = self.maps.lock().unwrap().cache_entries.remove(&cache_entry) {
            self.emit(Event::InstallCachePopulateComplete { id });
        }
        self.inner.on_populate_cache_complete(cache_entry);
    }

    fn on_unlink_start(&self, operation: usize, record: &PrefixRecord) -> usize {
        let inner_idx = self.inner.on_unlink_start(operation, record);
        let id = self.alloc();
        let parent = self
            .maps
            .lock()
            .unwrap()
            .operations
            .get(&operation)
            .copied()
            .unwrap_or(self.install_id);
        self.maps.lock().unwrap().unlinks.insert(inner_idx, id);
        self.emit(Event::InstallUnlinkStarted {
            id,
            parent,
            package: package_from_prefix(record),
        });
        inner_idx
    }

    fn on_unlink_complete(&self, index: usize) {
        if let Some(id) = self.maps.lock().unwrap().unlinks.remove(&index) {
            self.emit(Event::InstallUnlinkComplete { id });
        }
        self.inner.on_unlink_complete(index);
    }

    fn on_link_start(&self, operation: usize, record: &RepoDataRecord) -> usize {
        let inner_idx = self.inner.on_link_start(operation, record);
        let id = self.alloc();
        let parent = self
            .maps
            .lock()
            .unwrap()
            .operations
            .get(&operation)
            .copied()
            .unwrap_or(self.install_id);
        self.maps.lock().unwrap().links.insert(inner_idx, id);
        self.emit(Event::InstallLinkStarted {
            id,
            parent,
            package: package_from_repodata(record),
        });
        inner_idx
    }

    fn on_link_complete(&self, index: usize) {
        if let Some(id) = self.maps.lock().unwrap().links.remove(&index) {
            self.emit(Event::InstallLinkComplete { id });
        }
        self.inner.on_link_complete(index);
    }

    fn on_post_link_start(&self, package_name: &str, script_path: &str) -> usize {
        let inner_idx = self.inner.on_post_link_start(package_name, script_path);
        let id = self.alloc();
        self.maps.lock().unwrap().post_links.insert(inner_idx, id);
        self.emit(Event::InstallPostLinkStarted {
            id,
            parent: self.install_id,
            package: package_name.to_string(),
            script: script_path.to_string(),
        });
        inner_idx
    }

    fn on_post_link_complete(&self, index: usize, success: bool) {
        if let Some(id) = self.maps.lock().unwrap().post_links.remove(&index) {
            self.emit(Event::InstallPostLinkComplete { id, success });
        }
        self.inner.on_post_link_complete(index, success);
    }

    fn on_pre_unlink_start(&self, package_name: &str, script_path: &str) -> usize {
        let inner_idx = self.inner.on_pre_unlink_start(package_name, script_path);
        let id = self.alloc();
        self.maps.lock().unwrap().pre_unlinks.insert(inner_idx, id);
        self.emit(Event::InstallPreUnlinkStarted {
            id,
            parent: self.install_id,
            package: package_name.to_string(),
            script: script_path.to_string(),
        });
        inner_idx
    }

    fn on_pre_unlink_complete(&self, index: usize, success: bool) {
        if let Some(id) = self.maps.lock().unwrap().pre_unlinks.remove(&index) {
            self.emit(Event::InstallPreUnlinkComplete { id, success });
        }
        self.inner.on_pre_unlink_complete(index, success);
    }

    fn on_transaction_operation_complete(&self, operation: usize) {
        if let Some(id) = self.maps.lock().unwrap().operations.remove(&operation) {
            self.emit(Event::InstallOperationComplete { id });
        }
        self.inner.on_transaction_operation_complete(operation);
    }

    fn on_transaction_complete(&self) {
        self.emit(Event::InstallTransactionComplete {
            id: self.install_id,
        });
        self.inner.on_transaction_complete();
    }
}

impl PackageRef {
    fn unknown() -> Self {
        Self {
            name: String::new(),
            version: None,
            build: None,
            subdir: None,
            channel: None,
            size_bytes: None,
        }
    }
}

fn package_from_repodata(record: &RepoDataRecord) -> PackageRef {
    let pr = &record.package_record;
    PackageRef {
        name: pr.name.as_normalized().to_string(),
        version: Some(pr.version.to_string()),
        build: Some(pr.build.clone()),
        subdir: Some(pr.subdir.clone()),
        channel: Some(record.channel.clone().unwrap_or_default()),
        size_bytes: pr.size,
    }
}

fn package_from_prefix(record: &PrefixRecord) -> PackageRef {
    let pr = &record.repodata_record.package_record;
    PackageRef {
        name: pr.name.as_normalized().to_string(),
        version: Some(pr.version.to_string()),
        build: Some(pr.build.clone()),
        subdir: Some(pr.subdir.clone()),
        channel: Some(record.repodata_record.channel.clone().unwrap_or_default()),
        size_bytes: pr.size,
    }
}

fn transaction_op_summary(
    op: &rattler::install::TransactionOperation<PrefixRecord, RepoDataRecord>,
) -> TransactionOpSummary {
    use rattler::install::TransactionOperation as TO;
    match op {
        TO::Install(record) => {
            let pr = &record.package_record;
            TransactionOpSummary {
                kind: TransactionOpKind::Install,
                name: pr.name.as_normalized().to_string(),
                from_version: None,
                to_version: Some(pr.version.to_string()),
                size_bytes: pr.size,
                build: Some(pr.build.clone()),
                channel: Some(record.channel.clone().unwrap_or_default()),
                subdir: Some(pr.subdir.clone()),
            }
        }
        TO::Change { old, new } | TO::Reinstall { old, new } => {
            let kind = if matches!(op, TO::Change { .. }) {
                TransactionOpKind::Change
            } else {
                TransactionOpKind::Reinstall
            };
            let new_pr = &new.package_record;
            let old_pr = &old.repodata_record.package_record;
            TransactionOpSummary {
                kind,
                name: new_pr.name.as_normalized().to_string(),
                from_version: Some(old_pr.version.to_string()),
                to_version: Some(new_pr.version.to_string()),
                size_bytes: new_pr.size,
                build: Some(new_pr.build.clone()),
                channel: Some(new.channel.clone().unwrap_or_default()),
                subdir: Some(new_pr.subdir.clone()),
            }
        }
        TO::Remove(record) => {
            let pr = &record.repodata_record.package_record;
            TransactionOpSummary {
                kind: TransactionOpKind::Remove,
                name: pr.name.as_normalized().to_string(),
                from_version: Some(pr.version.to_string()),
                to_version: None,
                size_bytes: pr.size,
                build: Some(pr.build.clone()),
                channel: Some(record.repodata_record.channel.clone().unwrap_or_default()),
                subdir: Some(pr.subdir.clone()),
            }
        }
    }
}

fn transaction_op_index_summary(_operation_idx: usize) -> TransactionOpSummary {
    // We don't have access to the transaction at this point. The full
    // op summary was already emitted in InstallTransactionStarted; here
    // we only know the index. Emit a minimal summary referencing it.
    TransactionOpSummary {
        kind: TransactionOpKind::Install,
        name: String::new(),
        from_version: None,
        to_version: None,
        size_bytes: None,
        build: None,
        channel: None,
        subdir: None,
    }
}
