// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Defines the LIMIT plan

use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::stream::Stream;
use futures::stream::StreamExt;

use crate::error::{DataFusionError, Result};
use crate::physical_plan::{
    DisplayFormatType, Distribution, ExecutionPlan, Partitioning,
};
use crate::physical_plan::LambdaExecPlan;
use arrow::array::ArrayRef;
use arrow::compute::limit;
use arrow::datatypes::SchemaRef;
use arrow::error::Result as ArrowResult;
use arrow::record_batch::RecordBatch;

use super::{
    metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet},
    RecordBatchStream, SendableRecordBatchStream, Statistics,
};

use async_trait::async_trait;

use serde::{Deserialize, Serialize};

/// Limit execution plan
#[derive(Debug, Serialize, Deserialize)]
pub struct GlobalLimitExec {
    /// Input execution plan
    input: Arc<dyn ExecutionPlan>,
    /// Maximum number of rows to return
    limit: usize,
    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
}

impl GlobalLimitExec {
    /// Create a new GlobalLimitExec
    pub fn new(input: Arc<dyn ExecutionPlan>, limit: usize) -> Self {
        GlobalLimitExec {
            input,
            limit,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    /// Input execution plan
    pub fn input(&self) -> &Arc<dyn ExecutionPlan> {
        &self.input
    }

    /// Maximum number of rows to return
    pub fn limit(&self) -> usize {
        self.limit
    }
}

#[async_trait]
#[typetag::serde(name = "global_limit_exec")]
impl ExecutionPlan for GlobalLimitExec {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }

    fn required_child_distribution(&self) -> Distribution {
        Distribution::SinglePartition
    }

    /// Get the output partitioning of this plan
    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(1)
    }

    fn with_new_children(
        &self,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match children.len() {
            1 => Ok(Arc::new(GlobalLimitExec::new(
                children[0].clone(),
                self.limit,
            ))),
            _ => Err(DataFusionError::Internal(
                "GlobalLimitExec wrong number of children".to_string(),
            )),
        }
    }

    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        // GlobalLimitExec has a single output partition
        if 0 != partition {
            return Err(DataFusionError::Internal(format!(
                "GlobalLimitExec invalid partition {}",
                partition
            )));
        }

        // GlobalLimitExec requires a single input partition
        if 1 != self.input.output_partitioning().partition_count() {
            return Err(DataFusionError::Internal(
                "GlobalLimitExec requires a single input partition".to_owned(),
            ));
        }

        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);
        let stream = self.input.execute(0).await?;
        Ok(Box::pin(LimitStream::new(
            stream,
            self.limit,
            baseline_metrics,
        )))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "GlobalLimitExec: limit={}", self.limit)
            }
        }
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Statistics {
        let input_stats = self.input.statistics();
        match input_stats {
            // if the input does not reach the limit globally, return input stats
            Statistics {
                num_rows: Some(nr), ..
            } if nr <= self.limit => input_stats,
            // if the input is greater than the limit, the num_row will be the limit
            // but we won't be able to predict the other statistics
            Statistics {
                num_rows: Some(nr), ..
            } if nr > self.limit => Statistics {
                num_rows: Some(self.limit),
                is_exact: input_stats.is_exact,
                ..Default::default()
            },
            // if we don't know the input size, we can't predict the limit's behaviour
            _ => Statistics::default(),
        }
    }
}

#[async_trait]
impl LambdaExecPlan for GlobalLimitExec {
    fn feed_batches(&mut self, _partitions: Vec<Vec<RecordBatch>>) {
        unimplemented!();
    }
}

/// LocalLimitExec applies a limit to a single partition
#[derive(Debug, Serialize, Deserialize)]
pub struct LocalLimitExec {
    /// Input execution plan
    input: Arc<dyn ExecutionPlan>,
    /// Maximum number of rows to return
    limit: usize,
    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
}

impl LocalLimitExec {
    /// Create a new LocalLimitExec partition
    pub fn new(input: Arc<dyn ExecutionPlan>, limit: usize) -> Self {
        Self {
            input,
            limit,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    /// Input execution plan
    pub fn input(&self) -> &Arc<dyn ExecutionPlan> {
        &self.input
    }

    /// Maximum number of rows to return
    pub fn limit(&self) -> usize {
        self.limit
    }
}

#[async_trait]
#[typetag::serde(name = "local_limit_exec")]
impl ExecutionPlan for LocalLimitExec {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }

    fn output_partitioning(&self) -> Partitioning {
        self.input.output_partitioning()
    }

    fn with_new_children(
        &self,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match children.len() {
            1 => Ok(Arc::new(LocalLimitExec::new(
                children[0].clone(),
                self.limit,
            ))),
            _ => Err(DataFusionError::Internal(
                "LocalLimitExec wrong number of children".to_string(),
            )),
        }
    }

    async fn execute(&self, partition: usize) -> Result<SendableRecordBatchStream> {
        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);
        let stream = self.input.execute(partition).await?;
        Ok(Box::pin(LimitStream::new(
            stream,
            self.limit,
            baseline_metrics,
        )))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(f, "LocalLimitExec: limit={}", self.limit)
            }
        }
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Statistics {
        let input_stats = self.input.statistics();
        match input_stats {
            // if the input does not reach the limit globally, return input stats
            Statistics {
                num_rows: Some(nr), ..
            } if nr <= self.limit => input_stats,
            // if the input is greater than the limit, the num_row will be greater
            // than the limit because the partitions will be limited separatly
            // the statistic
            Statistics {
                num_rows: Some(nr), ..
            } if nr > self.limit => Statistics {
                num_rows: Some(self.limit),
                // this is not actually exact, but will be when GlobalLimit is applied
                // TODO stats: find a more explicit way to vehiculate this information
                is_exact: input_stats.is_exact,
                ..Default::default()
            },
            // if we don't know the input size, we can't predict the limit's behaviour
            _ => Statistics::default(),
        }
    }
}

#[async_trait]
impl LambdaExecPlan for LocalLimitExec {
    fn feed_batches(&mut self, _partitions: Vec<Vec<RecordBatch>>) {
        unimplemented!();
    }
}

/// Truncate a RecordBatch to maximum of n rows
pub fn truncate_batch(batch: &RecordBatch, n: usize) -> RecordBatch {
    let limited_columns: Vec<ArrayRef> = (0..batch.num_columns())
        .map(|i| limit(batch.column(i), n))
        .collect();

    RecordBatch::try_new(batch.schema(), limited_columns).unwrap()
}

/// A Limit stream limits the stream to up to `limit` rows.
struct LimitStream {
    /// The maximum number of rows to produce
    limit: usize,
    /// The input to read from. This is set to None once the limit is
    /// reached to enable early termination
    input: Option<SendableRecordBatchStream>,
    /// Copy of the input schema
    schema: SchemaRef,
    // the current number of rows which have been produced
    current_len: usize,
    /// Execution time metrics
    baseline_metrics: BaselineMetrics,
}

impl LimitStream {
    fn new(
        input: SendableRecordBatchStream,
        limit: usize,
        baseline_metrics: BaselineMetrics,
    ) -> Self {
        let schema = input.schema();
        Self {
            limit,
            input: Some(input),
            schema,
            current_len: 0,
            baseline_metrics,
        }
    }

    fn stream_limit(&mut self, batch: RecordBatch) -> Option<RecordBatch> {
        // records time on drop
        let _timer = self.baseline_metrics.elapsed_compute().timer();
        if self.current_len == self.limit {
            self.input = None; // clear input so it can be dropped early
            None
        } else if self.current_len + batch.num_rows() <= self.limit {
            self.current_len += batch.num_rows();
            Some(batch)
        } else {
            let batch_rows = self.limit - self.current_len;
            self.current_len = self.limit;
            self.input = None; // clear input so it can be dropped early
            Some(truncate_batch(&batch, batch_rows))
        }
    }
}

impl Stream for LimitStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let poll = match &mut self.input {
            Some(input) => input.poll_next_unpin(cx).map(|x| match x {
                Some(Ok(batch)) => Ok(self.stream_limit(batch)).transpose(),
                other => other,
            }),
            // input has been cleared
            None => Poll::Ready(None),
        };

        self.baseline_metrics.record_poll(poll)
    }
}

impl RecordBatchStream for LimitStream {
    /// Get the schema
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

#[cfg(test)]
mod tests {

    use common::collect;

    use super::*;
    use crate::datasource::object_store::local::LocalFileSystem;
    use crate::physical_plan::coalesce_partitions::CoalescePartitionsExec;
    use crate::physical_plan::common;
    use crate::physical_plan::file_format::{CsvExec, PhysicalPlanConfig};
    use crate::{test, test_util};

    #[tokio::test]
    async fn limit() -> Result<()> {
        let schema = test_util::aggr_test_schema();

        let num_partitions = 4;
        let (_, files) =
            test::create_partitioned_csv("aggregate_test_100.csv", num_partitions)?;

        let csv = CsvExec::new(
            PhysicalPlanConfig {
                object_store: Arc::new(LocalFileSystem {}),
                file_schema: schema,
                file_groups: files,
                statistics: Statistics::default(),
                projection: None,
                batch_size: 1024,
                limit: None,
                table_partition_cols: vec![],
            },
            true,
            b',',
        );

        // input should have 4 partitions
        assert_eq!(csv.output_partitioning().partition_count(), num_partitions);

        let limit =
            GlobalLimitExec::new(Arc::new(CoalescePartitionsExec::new(Arc::new(csv))), 7);

        // the result should contain 4 batches (one per input partition)
        let iter = limit.execute(0).await?;
        let batches = common::collect(iter).await?;

        // there should be a total of 100 rows
        let row_count: usize = batches.iter().map(|batch| batch.num_rows()).sum();
        assert_eq!(row_count, 7);

        Ok(())
    }

    #[tokio::test]
    async fn limit_early_shutdown() -> Result<()> {
        let batches = vec![
            test::make_partition(5),
            test::make_partition(10),
            test::make_partition(15),
            test::make_partition(20),
            test::make_partition(25),
        ];
        let input = test::exec::TestStream::new(batches);

        let index = input.index();
        assert_eq!(index.value(), 0);

        // limit of six needs to consume the entire first record batch
        // (5 rows) and 1 row from the second (1 row)
        let baseline_metrics = BaselineMetrics::new(&ExecutionPlanMetricsSet::new(), 0);
        let limit_stream = LimitStream::new(Box::pin(input), 6, baseline_metrics);
        assert_eq!(index.value(), 0);

        let results = collect(Box::pin(limit_stream)).await.unwrap();
        let num_rows: usize = results.into_iter().map(|b| b.num_rows()).sum();
        // Only 6 rows should have been produced
        assert_eq!(num_rows, 6);

        // Only the first two batches should be consumed
        assert_eq!(index.value(), 2);

        Ok(())
    }
}
