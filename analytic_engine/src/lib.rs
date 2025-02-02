// Copyright 2022-2023 CeresDB Project Authors. Licensed under Apache-2.0.

//! Analytic table engine implementations

#![feature(option_get_or_insert_default)]
mod compaction;
mod context;
mod engine;
mod instance;
mod manifest;
pub mod memtable;
mod payload;
pub mod row_iter;
mod sampler;
pub mod setup;
pub mod space;
pub mod sst;
pub mod table;
pub mod table_options;

pub mod table_meta_set_impl;
#[cfg(any(test, feature = "test"))]
pub mod tests;

use common_util::config::{ReadableDuration, ReadableSize};
use manifest::details::Options as ManifestOptions;
use message_queue::kafka::config::Config as KafkaConfig;
use object_store::config::StorageOptions;
use serde::{Deserialize, Serialize};
use table_kv::config::ObkvConfig;
use wal::{
    message_queue_impl::config::Config as MessageQueueWalConfig,
    rocks_impl::config::Config as RocksDBWalConfig, table_kv_impl::model::NamespaceConfig,
};

pub use crate::{compaction::scheduler::SchedulerConfig, table_options::TableOptions};

/// Config of analytic engine
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Storage options of the engine
    pub storage: StorageOptions,

    /// Batch size to read records from wal to replay
    pub replay_batch_size: usize,
    /// Batch size to replay tables
    pub max_replay_tables_per_batch: usize,

    /// Default options for table
    pub table_opts: TableOptions,

    pub compaction: SchedulerConfig,

    /// sst meta cache capacity
    pub sst_meta_cache_cap: Option<usize>,
    /// sst data cache capacity
    pub sst_data_cache_cap: Option<usize>,

    /// Manifest options
    pub manifest: ManifestOptions,

    /// The maximum rows in the write queue.
    pub max_rows_in_write_queue: usize,
    /// The maximum write buffer size used for single space.
    pub space_write_buffer_size: usize,
    /// The maximum size of all Write Buffers across all spaces.
    pub db_write_buffer_size: usize,
    /// The ratio of table's write buffer size to trigger preflush, and it
    /// should be in the range (0, 1].
    pub preflush_write_buffer_size_ratio: f32,

    // Iterator scanning options
    /// Batch size for iterator.
    ///
    /// The `num_rows_per_row_group` in `table options` will be used if this is
    /// not set.
    pub scan_batch_size: Option<usize>,
    /// Max record batches in flight when scan
    pub scan_max_record_batches_in_flight: usize,
    /// Sst background reading parallelism
    pub sst_background_read_parallelism: usize,
    /// Max buffer size for writing sst
    pub write_sst_max_buffer_size: ReadableSize,
    /// Max retry limit After flush failed
    pub max_retry_flush_limit: usize,
    /// Max bytes per write batch.
    ///
    /// If this is set, the atomicity of write request will be broken.
    pub max_bytes_per_write_batch: Option<ReadableSize>,

    /// Wal storage config
    ///
    /// Now, following three storages are supported:
    /// + RocksDB
    /// + OBKV
    /// + Kafka
    pub wal: WalStorageConfig,

    /// Recover mode
    ///
    /// + TableBased, tables on same shard will be recovered table by table.
    /// + ShardBased, tables on same shard will be recovered together.
    pub recover_mode: RecoverMode,

    pub remote_engine_client: remote_engine_client::config::Config,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub enum RecoverMode {
    TableBased,
    ShardBased,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storage: Default::default(),
            replay_batch_size: 500,
            max_replay_tables_per_batch: 64,
            table_opts: TableOptions::default(),
            compaction: SchedulerConfig::default(),
            sst_meta_cache_cap: Some(1000),
            sst_data_cache_cap: Some(1000),
            manifest: ManifestOptions::default(),
            max_rows_in_write_queue: 0,
            /// Zero means disabling this param, give a positive value to enable
            /// it.
            space_write_buffer_size: 0,
            /// Zero means disabling this param, give a positive value to enable
            /// it.
            db_write_buffer_size: 0,
            preflush_write_buffer_size_ratio: 0.75,
            scan_batch_size: None,
            sst_background_read_parallelism: 8,
            scan_max_record_batches_in_flight: 1024,
            write_sst_max_buffer_size: ReadableSize::mb(10),
            max_retry_flush_limit: 0,
            max_bytes_per_write_batch: None,
            wal: WalStorageConfig::RocksDB(Box::default()),
            remote_engine_client: remote_engine_client::config::Config::default(),
            recover_mode: RecoverMode::TableBased,
        }
    }
}

/// Config of wal based on obkv
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ObkvWalConfig {
    /// Obkv client config
    pub obkv: ObkvConfig,
    /// Namespace config for data.
    pub data_namespace: WalNamespaceConfig,
    /// Namespace config for meta data
    pub meta_namespace: ManifestNamespaceConfig,
}

/// Config of obkv wal based manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestNamespaceConfig {
    /// Decide how many wal data shards will be created
    ///
    /// NOTICE: it can just be set once, the later setting makes no effect.
    pub shard_num: usize,

    /// Decide how many wal meta shards will be created
    ///
    /// NOTICE: it can just be set once, the later setting makes no effect.
    pub meta_shard_num: usize,

    pub init_scan_timeout: ReadableDuration,
    pub init_scan_batch_size: i32,
    pub clean_scan_timeout: ReadableDuration,
    pub clean_scan_batch_size: usize,
}

impl Default for ManifestNamespaceConfig {
    fn default() -> Self {
        let namespace_config = NamespaceConfig::default();

        Self {
            shard_num: namespace_config.wal_shard_num,
            meta_shard_num: namespace_config.table_unit_meta_shard_num,
            init_scan_timeout: namespace_config.init_scan_timeout,
            init_scan_batch_size: namespace_config.init_scan_batch_size,
            clean_scan_timeout: namespace_config.clean_scan_timeout,
            clean_scan_batch_size: namespace_config.clean_scan_batch_size,
        }
    }
}

impl From<ManifestNamespaceConfig> for NamespaceConfig {
    fn from(manifest_config: ManifestNamespaceConfig) -> Self {
        NamespaceConfig {
            wal_shard_num: manifest_config.shard_num,
            table_unit_meta_shard_num: manifest_config.meta_shard_num,
            ttl: None,
            init_scan_timeout: manifest_config.init_scan_timeout,
            init_scan_batch_size: manifest_config.init_scan_batch_size,
            clean_scan_timeout: manifest_config.clean_scan_timeout,
            clean_scan_batch_size: manifest_config.clean_scan_batch_size,
        }
    }
}

/// Config of obkv wal based wal module
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WalNamespaceConfig {
    /// Decide how many wal data shards will be created
    ///
    /// NOTICE: it can just be set once, the later setting makes no effect.
    pub shard_num: usize,

    /// Decide how many wal meta shards will be created
    ///
    /// NOTICE: it can just be set once, the later setting makes no effect.
    pub meta_shard_num: usize,

    pub ttl: ReadableDuration,
    pub init_scan_timeout: ReadableDuration,
    pub init_scan_batch_size: i32,
}

impl Default for WalNamespaceConfig {
    fn default() -> Self {
        let namespace_config = NamespaceConfig::default();

        Self {
            shard_num: namespace_config.wal_shard_num,
            meta_shard_num: namespace_config.table_unit_meta_shard_num,
            ttl: namespace_config.ttl.unwrap(),
            init_scan_timeout: namespace_config.init_scan_timeout,
            init_scan_batch_size: namespace_config.init_scan_batch_size,
        }
    }
}

impl From<WalNamespaceConfig> for NamespaceConfig {
    fn from(wal_config: WalNamespaceConfig) -> Self {
        Self {
            wal_shard_num: wal_config.shard_num,
            table_unit_meta_shard_num: wal_config.meta_shard_num,
            ttl: Some(wal_config.ttl),
            init_scan_timeout: wal_config.init_scan_timeout,
            init_scan_batch_size: wal_config.init_scan_batch_size,
            ..Default::default()
        }
    }
}

/// Config of wal based on obkv
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct KafkaWalConfig {
    /// Kafka client config
    pub kafka: KafkaConfig,

    /// Namespace config for data.
    pub data_namespace: MessageQueueWalConfig,
    /// Namespace config for meta data
    pub meta_namespace: MessageQueueWalConfig,
}

/// Config for wal based on RocksDB
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RocksDBConfig {
    /// Data directory used by RocksDB.
    pub data_dir: String,

    pub data_namespace: RocksDBWalConfig,
    pub meta_namespace: RocksDBWalConfig,
}

impl Default for RocksDBConfig {
    fn default() -> Self {
        Self {
            data_dir: "/tmp/ceresdb".to_string(),
            data_namespace: Default::default(),
            meta_namespace: Default::default(),
        }
    }
}
/// Options for wal storage backend
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum WalStorageConfig {
    RocksDB(Box<RocksDBConfig>),
    Obkv(Box<ObkvWalConfig>),
    Kafka(Box<KafkaWalConfig>),
}
