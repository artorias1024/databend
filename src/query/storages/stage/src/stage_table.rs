// Copyright 2021 Datafuse Labs
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
use std::ops::Deref;
use std::sync::Arc;

use databend_common_catalog::plan::DataSourceInfo;
use databend_common_catalog::plan::DataSourcePlan;
use databend_common_catalog::plan::PartInfo;
use databend_common_catalog::plan::PartStatistics;
use databend_common_catalog::plan::Partitions;
use databend_common_catalog::plan::PartitionsShuffleKind;
use databend_common_catalog::plan::PushDownInfo;
use databend_common_catalog::plan::StageTableInfo;
use databend_common_catalog::table::AppendMode;
use databend_common_catalog::table::Table;
use databend_common_catalog::table_context::TableContext;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::BlockThresholds;
use databend_common_meta_app::principal::FileFormatParams;
use databend_common_meta_app::principal::StageInfo;
use databend_common_meta_app::schema::TableInfo;
use databend_common_pipeline_core::Pipeline;
use databend_common_storage::init_stage_operator;
use databend_common_storage::StageFileInfo;
use databend_common_storages_parquet::ParquetTableForCopy;
use opendal::Operator;
use parking_lot::Mutex;

use crate::one_file_partition::OneFilePartition;
use crate::read::row_based::RowBasedReadPipelineBuilder;

/// TODO: we need to track the data metrics in stage table.
pub struct StageTable {
    pub(crate) table_info: StageTableInfo,
    // This is no used but a placeholder.
    // But the Table trait need it:
    // fn get_table_info(&self) -> &TableInfo).
    table_info_placeholder: TableInfo,
    block_compact_threshold: Mutex<Option<BlockThresholds>>,
}

impl StageTable {
    pub fn try_create(table_info: StageTableInfo) -> Result<Arc<dyn Table>> {
        let table_info_placeholder = TableInfo {
            ident: Default::default(),
            desc: "".to_string(),
            // `system.stage` is used to forbidden the user to select * from text files.
            name: "stage".to_string(),
            meta: Default::default(),
            tenant: "".to_string(),
            db_type: Default::default(),
        }
        .set_schema(table_info.schema());

        Ok(Arc::new(Self {
            table_info,
            table_info_placeholder,
            block_compact_threshold: Default::default(),
        }))
    }

    /// Get operator with correctly prefix.
    pub fn get_op(stage: &StageInfo) -> Result<Operator> {
        init_stage_operator(stage)
    }

    #[async_backtrace::framed]
    pub async fn list_files(
        stage_info: &StageTableInfo,
        max_files: Option<usize>,
    ) -> Result<Vec<StageFileInfo>> {
        stage_info.list_files(max_files).await
    }

    pub fn get_block_compact_thresholds_with_default(&self) -> BlockThresholds {
        let guard = self.block_compact_threshold.lock();
        guard.deref().unwrap_or_default()
    }

    pub async fn read_partitions_simple(
        &self,
        stage_table_info: &StageTableInfo,
    ) -> Result<(PartStatistics, Partitions)> {
        let files = if let Some(files) = &stage_table_info.files_to_copy {
            files.clone()
        } else {
            StageTable::list_files(stage_table_info, None).await?
        };
        let size = files.iter().map(|f| f.size as usize).sum();
        // assuming all fields are empty
        let max_rows = std::cmp::max(size / (stage_table_info.schema.fields.len() + 1), 1);
        let statistics = PartStatistics {
            snapshot: None,
            read_rows: max_rows,
            read_bytes: size,
            partitions_scanned: files.len(),
            partitions_total: files.len(),
            is_exact: false,
            pruning_stats: Default::default(),
        };

        let partitions = files
            .into_iter()
            .map(|v| {
                let part = OneFilePartition {
                    path: v.path.clone(),
                    size: v.size as usize,
                };
                let part_info: Box<dyn PartInfo> = Box::new(part);
                Arc::new(part_info)
            })
            .collect::<Vec<_>>();

        Ok((
            statistics,
            Partitions::create_nolazy(PartitionsShuffleKind::Seq, partitions),
        ))
    }
}

#[async_trait::async_trait]
impl Table for StageTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    // External stage has no table info yet.
    fn get_table_info(&self) -> &TableInfo {
        &self.table_info_placeholder
    }

    fn get_data_source_info(&self) -> DataSourceInfo {
        DataSourceInfo::StageSource(self.table_info.clone())
    }

    #[async_backtrace::framed]
    async fn read_partitions(
        &self,
        ctx: Arc<dyn TableContext>,
        _push_downs: Option<PushDownInfo>,
        _dry_run: bool,
    ) -> Result<(PartStatistics, Partitions)> {
        let settings = ctx.get_settings();
        let stage_table_info = &self.table_info;
        match stage_table_info.stage_info.file_format_params {
            FileFormatParams::Parquet(_) => {
                ParquetTableForCopy::do_read_partitions(stage_table_info, ctx, _push_downs).await
            }
            FileFormatParams::Csv(_) if settings.get_enable_new_copy_for_text_formats()? == 1 => {
                self.read_partitions_simple(stage_table_info).await
            }
            _ => self.read_partition_old(&ctx).await,
        }
    }

    fn read_data(
        &self,
        ctx: Arc<dyn TableContext>,
        plan: &DataSourcePlan,
        pipeline: &mut Pipeline,
        _put_cache: bool,
    ) -> Result<()> {
        let settings = ctx.get_settings();
        let stage_table_info =
            if let DataSourceInfo::StageSource(stage_table_info) = &plan.source_info {
                stage_table_info
            } else {
                return Err(ErrorCode::Internal(""));
            };
        match stage_table_info.stage_info.file_format_params {
            FileFormatParams::Parquet(_) => {
                ParquetTableForCopy::do_read_data(ctx, plan, pipeline, _put_cache)
            }
            FileFormatParams::Csv(_) if settings.get_enable_new_copy_for_text_formats()? == 1 => {
                let compact_threshold = self.get_block_compact_thresholds_with_default();
                RowBasedReadPipelineBuilder {
                    stage_table_info,
                    compact_threshold,
                }
                .read_data(ctx, plan, pipeline)
            }
            _ => self.read_data_old(ctx, plan, pipeline, stage_table_info),
        }
    }

    fn append_data(
        &self,
        ctx: Arc<dyn TableContext>,
        pipeline: &mut Pipeline,
        _: AppendMode,
    ) -> Result<()> {
        self.do_append_data(ctx, pipeline)
    }

    // Truncate the stage file.
    #[async_backtrace::framed]
    async fn truncate(&self, _ctx: Arc<dyn TableContext>) -> Result<()> {
        Err(ErrorCode::Unimplemented(
            "S3 external table truncate() unimplemented yet!",
        ))
    }

    fn get_block_thresholds(&self) -> BlockThresholds {
        let guard = self.block_compact_threshold.lock();
        (*guard).expect("must success")
    }

    fn set_block_thresholds(&self, thresholds: BlockThresholds) {
        let mut guard = self.block_compact_threshold.lock();
        (*guard) = Some(thresholds)
    }
}
