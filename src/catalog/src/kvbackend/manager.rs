// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::any::Any;
use std::collections::BTreeSet;
use std::sync::{Arc, Weak};

use async_stream::try_stream;
use common_catalog::consts::{
    DEFAULT_CATALOG_NAME, DEFAULT_SCHEMA_NAME, INFORMATION_SCHEMA_NAME, NUMBERS_TABLE_ID,
    PG_CATALOG_NAME,
};
use common_error::ext::BoxedError;
use common_meta::cache::{
    LayeredCacheRegistryRef, TableRoute, TableRouteCacheRef, ViewInfoCacheRef,
};
use common_meta::key::catalog_name::CatalogNameKey;
use common_meta::key::flow::FlowMetadataManager;
use common_meta::key::schema_name::SchemaNameKey;
use common_meta::key::table_info::{TableInfoManager, TableInfoValue};
use common_meta::key::table_name::TableNameKey;
use common_meta::key::TableMetadataManagerRef;
use common_meta::kv_backend::KvBackendRef;
use common_procedure::ProcedureManagerRef;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use moka::sync::Cache;
use partition::manager::PartitionRuleManagerRef;
use session::context::{Channel, QueryContext};
use snafu::prelude::*;
use store_api::metric_engine_consts::METRIC_ENGINE_NAME;
use table::dist_table::DistTable;
use table::metadata::TableId;
use table::table::numbers::{NumbersTable, NUMBERS_TABLE_NAME};
use table::table_name::TableName;
use table::TableRef;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{
    CacheNotFoundSnafu, GetTableCacheSnafu, InvalidTableInfoInCatalogSnafu, ListCatalogsSnafu,
    ListSchemasSnafu, ListTablesSnafu, Result, TableMetadataManagerSnafu,
};
#[cfg(feature = "enterprise")]
use crate::information_schema::InformationSchemaTableFactoryRef;
use crate::information_schema::{InformationExtensionRef, InformationSchemaProvider};
use crate::kvbackend::TableCacheRef;
use crate::process_manager::ProcessManagerRef;
use crate::system_schema::pg_catalog::PGCatalogProvider;
use crate::system_schema::SystemSchemaProvider;
use crate::CatalogManager;

/// Access all existing catalog, schema and tables.
///
/// The result comes from two source, all the user tables are presented in
/// a kv-backend which persists the metadata of a table. And system tables
/// comes from `SystemCatalog`, which is static and read-only.
#[derive(Clone)]
pub struct KvBackendCatalogManager {
    /// Provides the extension methods for the `information_schema` tables
    pub(super) information_extension: InformationExtensionRef,
    /// Manages partition rules.
    pub(super) partition_manager: PartitionRuleManagerRef,
    /// Manages table metadata.
    pub(super) table_metadata_manager: TableMetadataManagerRef,
    /// A sub-CatalogManager that handles system tables
    pub(super) system_catalog: SystemCatalog,
    /// Cache registry for all caches.
    pub(super) cache_registry: LayeredCacheRegistryRef,
    /// Only available in `Standalone` mode.
    pub(super) procedure_manager: Option<ProcedureManagerRef>,
}

pub(super) const CATALOG_CACHE_MAX_CAPACITY: u64 = 128;

impl KvBackendCatalogManager {
    pub fn view_info_cache(&self) -> Result<ViewInfoCacheRef> {
        self.cache_registry.get().context(CacheNotFoundSnafu {
            name: "view_info_cache",
        })
    }

    /// Returns the [`InformationExtension`].
    pub fn information_extension(&self) -> InformationExtensionRef {
        self.information_extension.clone()
    }

    pub fn partition_manager(&self) -> PartitionRuleManagerRef {
        self.partition_manager.clone()
    }

    pub fn table_metadata_manager_ref(&self) -> &TableMetadataManagerRef {
        &self.table_metadata_manager
    }

    pub fn procedure_manager(&self) -> Option<ProcedureManagerRef> {
        self.procedure_manager.clone()
    }

    // Override logical table's partition key indices with physical table's.
    async fn override_logical_table_partition_key_indices(
        table_route_cache: &TableRouteCacheRef,
        table_info_manager: &TableInfoManager,
        table: TableRef,
    ) -> Result<TableRef> {
        // If the table is not a metric table, return the table directly.
        if table.table_info().meta.engine != METRIC_ENGINE_NAME {
            return Ok(table);
        }

        if let Some(table_route_value) = table_route_cache
            .get(table.table_info().table_id())
            .await
            .context(TableMetadataManagerSnafu)?
            && let TableRoute::Logical(logical_route) = &*table_route_value
            && let Some(physical_table_info_value) = table_info_manager
                .get(logical_route.physical_table_id())
                .await
                .context(TableMetadataManagerSnafu)?
        {
            let mut new_table_info = (*table.table_info()).clone();

            // Remap partition key indices from physical table to logical table
            new_table_info.meta.partition_key_indices = physical_table_info_value
                .table_info
                .meta
                .partition_key_indices
                .iter()
                .filter_map(|&physical_index| {
                    // Get the column name from the physical table using the physical index
                    physical_table_info_value
                        .table_info
                        .meta
                        .schema
                        .column_schemas
                        .get(physical_index)
                        .and_then(|physical_column| {
                            // Find the corresponding index in the logical table schema
                            new_table_info
                                .meta
                                .schema
                                .column_index_by_name(physical_column.name.as_str())
                        })
                })
                .collect();

            let new_table = DistTable::table(Arc::new(new_table_info));

            return Ok(new_table);
        }

        Ok(table)
    }
}

#[async_trait::async_trait]
impl CatalogManager for KvBackendCatalogManager {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn catalog_names(&self) -> Result<Vec<String>> {
        let stream = self
            .table_metadata_manager
            .catalog_manager()
            .catalog_names();

        let keys = stream
            .try_collect::<Vec<_>>()
            .await
            .map_err(BoxedError::new)
            .context(ListCatalogsSnafu)?;

        Ok(keys)
    }

    async fn schema_names(
        &self,
        catalog: &str,
        query_ctx: Option<&QueryContext>,
    ) -> Result<Vec<String>> {
        let stream = self
            .table_metadata_manager
            .schema_manager()
            .schema_names(catalog);
        let mut keys = stream
            .try_collect::<BTreeSet<_>>()
            .await
            .map_err(BoxedError::new)
            .context(ListSchemasSnafu { catalog })?;

        keys.extend(self.system_catalog.schema_names(query_ctx));

        Ok(keys.into_iter().collect())
    }

    async fn table_names(
        &self,
        catalog: &str,
        schema: &str,
        query_ctx: Option<&QueryContext>,
    ) -> Result<Vec<String>> {
        let mut tables = self
            .table_metadata_manager
            .table_name_manager()
            .tables(catalog, schema)
            .map_ok(|(table_name, _)| table_name)
            .try_collect::<Vec<_>>()
            .await
            .map_err(BoxedError::new)
            .context(ListTablesSnafu { catalog, schema })?;

        tables.extend(self.system_catalog.table_names(schema, query_ctx));
        Ok(tables)
    }

    async fn catalog_exists(&self, catalog: &str) -> Result<bool> {
        self.table_metadata_manager
            .catalog_manager()
            .exists(CatalogNameKey::new(catalog))
            .await
            .context(TableMetadataManagerSnafu)
    }

    async fn schema_exists(
        &self,
        catalog: &str,
        schema: &str,
        query_ctx: Option<&QueryContext>,
    ) -> Result<bool> {
        if self.system_catalog.schema_exists(schema, query_ctx) {
            return Ok(true);
        }

        self.table_metadata_manager
            .schema_manager()
            .exists(SchemaNameKey::new(catalog, schema))
            .await
            .context(TableMetadataManagerSnafu)
    }

    async fn table_exists(
        &self,
        catalog: &str,
        schema: &str,
        table: &str,
        query_ctx: Option<&QueryContext>,
    ) -> Result<bool> {
        if self.system_catalog.table_exists(schema, table, query_ctx) {
            return Ok(true);
        }

        let key = TableNameKey::new(catalog, schema, table);
        self.table_metadata_manager
            .table_name_manager()
            .get(key)
            .await
            .context(TableMetadataManagerSnafu)
            .map(|x| x.is_some())
    }

    async fn table(
        &self,
        catalog_name: &str,
        schema_name: &str,
        table_name: &str,
        query_ctx: Option<&QueryContext>,
    ) -> Result<Option<TableRef>> {
        let channel = query_ctx.map_or(Channel::Unknown, |ctx| ctx.channel());
        if let Some(table) =
            self.system_catalog
                .table(catalog_name, schema_name, table_name, query_ctx)
        {
            return Ok(Some(table));
        }

        let table_cache: TableCacheRef = self.cache_registry.get().context(CacheNotFoundSnafu {
            name: "table_cache",
        })?;

        let table = table_cache
            .get_by_ref(&TableName {
                catalog_name: catalog_name.to_string(),
                schema_name: schema_name.to_string(),
                table_name: table_name.to_string(),
            })
            .await
            .context(GetTableCacheSnafu)?;

        if let Some(table) = table {
            let table_route_cache: TableRouteCacheRef =
                self.cache_registry.get().context(CacheNotFoundSnafu {
                    name: "table_route_cache",
                })?;
            return Self::override_logical_table_partition_key_indices(
                &table_route_cache,
                self.table_metadata_manager.table_info_manager(),
                table,
            )
            .await
            .map(Some);
        }

        if channel == Channel::Postgres {
            // falldown to pg_catalog
            if let Some(table) =
                self.system_catalog
                    .table(catalog_name, PG_CATALOG_NAME, table_name, query_ctx)
            {
                return Ok(Some(table));
            }
        }

        Ok(None)
    }

    async fn tables_by_ids(
        &self,
        catalog: &str,
        schema: &str,
        table_ids: &[TableId],
    ) -> Result<Vec<TableRef>> {
        let table_info_values = self
            .table_metadata_manager
            .table_info_manager()
            .batch_get(table_ids)
            .await
            .context(TableMetadataManagerSnafu)?;

        let tables = table_info_values
            .into_values()
            .filter(|t| t.table_info.catalog_name == catalog && t.table_info.schema_name == schema)
            .map(build_table)
            .collect::<Result<Vec<_>>>()?;

        Ok(tables)
    }

    fn tables<'a>(
        &'a self,
        catalog: &'a str,
        schema: &'a str,
        query_ctx: Option<&'a QueryContext>,
    ) -> BoxStream<'a, Result<TableRef>> {
        let sys_tables = try_stream!({
            // System tables
            let sys_table_names = self.system_catalog.table_names(schema, query_ctx);
            for table_name in sys_table_names {
                if let Some(table) =
                    self.system_catalog
                        .table(catalog, schema, &table_name, query_ctx)
                {
                    yield table;
                }
            }
        });

        const BATCH_SIZE: usize = 128;
        const CONCURRENCY: usize = 8;

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let metadata_manager = self.table_metadata_manager.clone();
        let catalog = catalog.to_string();
        let schema = schema.to_string();
        let semaphore = Arc::new(Semaphore::new(CONCURRENCY));
        let table_route_cache: Result<TableRouteCacheRef> =
            self.cache_registry.get().context(CacheNotFoundSnafu {
                name: "table_route_cache",
            });

        common_runtime::spawn_global(async move {
            let table_route_cache = match table_route_cache {
                Ok(table_route_cache) => table_route_cache,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            let table_id_stream = metadata_manager
                .table_name_manager()
                .tables(&catalog, &schema)
                .map_ok(|(_, v)| v.table_id());
            // Split table ids into chunks
            let mut table_id_chunks = table_id_stream.ready_chunks(BATCH_SIZE);

            while let Some(table_ids) = table_id_chunks.next().await {
                let table_ids = match table_ids
                    .into_iter()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(BoxedError::new)
                    .context(ListTablesSnafu {
                        catalog: &catalog,
                        schema: &schema,
                    }) {
                    Ok(table_ids) => table_ids,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };

                let metadata_manager = metadata_manager.clone();
                let tx = tx.clone();
                let semaphore = semaphore.clone();
                let table_route_cache = table_route_cache.clone();
                common_runtime::spawn_global(async move {
                    // we don't explicitly close the semaphore so just ignore the potential error.
                    let _ = semaphore.acquire().await;
                    let table_info_values = match metadata_manager
                        .table_info_manager()
                        .batch_get(&table_ids)
                        .await
                        .context(TableMetadataManagerSnafu)
                    {
                        Ok(table_info_values) => table_info_values,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };

                    for table in table_info_values.into_values().map(build_table) {
                        let table = if let Ok(table) = table {
                            Self::override_logical_table_partition_key_indices(
                                &table_route_cache,
                                metadata_manager.table_info_manager(),
                                table,
                            )
                            .await
                        } else {
                            table
                        };
                        if tx.send(table).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let user_tables = ReceiverStream::new(rx);
        Box::pin(sys_tables.chain(user_tables))
    }
}

fn build_table(table_info_value: TableInfoValue) -> Result<TableRef> {
    let table_info = table_info_value
        .table_info
        .try_into()
        .context(InvalidTableInfoInCatalogSnafu)?;
    Ok(DistTable::table(Arc::new(table_info)))
}

// TODO: This struct can hold a static map of all system tables when
// the upper layer (e.g., procedure) can inform the catalog manager
// a new catalog is created.
/// Existing system tables:
/// - public.numbers
/// - information_schema.{tables}
/// - pg_catalog.{tables}
#[derive(Clone)]
pub(super) struct SystemCatalog {
    pub(super) catalog_manager: Weak<KvBackendCatalogManager>,
    pub(super) catalog_cache: Cache<String, Arc<InformationSchemaProvider>>,
    pub(super) pg_catalog_cache: Cache<String, Arc<PGCatalogProvider>>,

    // system_schema_provider for default catalog
    pub(super) information_schema_provider: Arc<InformationSchemaProvider>,
    pub(super) pg_catalog_provider: Arc<PGCatalogProvider>,
    pub(super) backend: KvBackendRef,
    pub(super) process_manager: Option<ProcessManagerRef>,
    #[cfg(feature = "enterprise")]
    pub(super) extra_information_table_factories:
        std::collections::HashMap<String, InformationSchemaTableFactoryRef>,
}

impl SystemCatalog {
    fn schema_names(&self, query_ctx: Option<&QueryContext>) -> Vec<String> {
        let channel = query_ctx.map_or(Channel::Unknown, |ctx| ctx.channel());
        match channel {
            // pg_catalog only visible under postgres protocol
            Channel::Postgres => vec![
                INFORMATION_SCHEMA_NAME.to_string(),
                PG_CATALOG_NAME.to_string(),
            ],
            _ => {
                vec![INFORMATION_SCHEMA_NAME.to_string()]
            }
        }
    }

    fn table_names(&self, schema: &str, query_ctx: Option<&QueryContext>) -> Vec<String> {
        let channel = query_ctx.map_or(Channel::Unknown, |ctx| ctx.channel());
        match schema {
            INFORMATION_SCHEMA_NAME => self.information_schema_provider.table_names(),
            PG_CATALOG_NAME if channel == Channel::Postgres => {
                self.pg_catalog_provider.table_names()
            }
            DEFAULT_SCHEMA_NAME => {
                vec![NUMBERS_TABLE_NAME.to_string()]
            }
            _ => vec![],
        }
    }

    fn schema_exists(&self, schema: &str, query_ctx: Option<&QueryContext>) -> bool {
        let channel = query_ctx.map_or(Channel::Unknown, |ctx| ctx.channel());
        match channel {
            Channel::Postgres => schema == PG_CATALOG_NAME || schema == INFORMATION_SCHEMA_NAME,
            _ => schema == INFORMATION_SCHEMA_NAME,
        }
    }

    fn table_exists(&self, schema: &str, table: &str, query_ctx: Option<&QueryContext>) -> bool {
        let channel = query_ctx.map_or(Channel::Unknown, |ctx| ctx.channel());
        if schema == INFORMATION_SCHEMA_NAME {
            self.information_schema_provider.table(table).is_some()
        } else if schema == DEFAULT_SCHEMA_NAME {
            table == NUMBERS_TABLE_NAME
        } else if schema == PG_CATALOG_NAME && channel == Channel::Postgres {
            self.pg_catalog_provider.table(table).is_some()
        } else {
            false
        }
    }

    fn table(
        &self,
        catalog: &str,
        schema: &str,
        table_name: &str,
        query_ctx: Option<&QueryContext>,
    ) -> Option<TableRef> {
        let channel = query_ctx.map_or(Channel::Unknown, |ctx| ctx.channel());
        if schema == INFORMATION_SCHEMA_NAME {
            let information_schema_provider =
                self.catalog_cache.get_with_by_ref(catalog, move || {
                    let provider = InformationSchemaProvider::new(
                        catalog.to_string(),
                        self.catalog_manager.clone(),
                        Arc::new(FlowMetadataManager::new(self.backend.clone())),
                        self.process_manager.clone(),
                        self.backend.clone(),
                    );
                    #[cfg(feature = "enterprise")]
                    let provider = provider
                        .with_extra_table_factories(self.extra_information_table_factories.clone());
                    Arc::new(provider)
                });
            information_schema_provider.table(table_name)
        } else if schema == PG_CATALOG_NAME && channel == Channel::Postgres {
            if catalog == DEFAULT_CATALOG_NAME {
                self.pg_catalog_provider.table(table_name)
            } else {
                let pg_catalog_provider =
                    self.pg_catalog_cache.get_with_by_ref(catalog, move || {
                        Arc::new(PGCatalogProvider::new(
                            catalog.to_string(),
                            self.catalog_manager.clone(),
                        ))
                    });
                pg_catalog_provider.table(table_name)
            }
        } else if schema == DEFAULT_SCHEMA_NAME && table_name == NUMBERS_TABLE_NAME {
            Some(NumbersTable::table(NUMBERS_TABLE_ID))
        } else {
            None
        }
    }
}
