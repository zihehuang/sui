// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

/// This module executes enriched benchmark queries against the database.
/// Each query's execution is timed and recorded via MetricsCollector.
/// And the results are aggregated and reported via BenchmarkResult.
use std::time::Instant;

use anyhow::{Context, Result};
use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use sui_indexer_alt_framework::task::TrySpawnStreamExt;
use tokio_postgres::{types::ToSql, NoTls};
use tracing::info;
use url::Url;

use crate::config::BenchmarkConfig;
use crate::direct::metrics::{BenchmarkResult, MetricsCollector};
use crate::direct::query_enricher::{EnrichedBenchmarkQuery, SqlValue};

pub struct QueryExecutor {
    pool: Pool<PostgresConnectionManager<NoTls>>,
    enriched_queries: Vec<EnrichedBenchmarkQuery>,
    config: BenchmarkConfig,
    metrics: MetricsCollector,
}

impl QueryExecutor {
    pub async fn new(
        db_url: &Url,
        enriched_queries: Vec<EnrichedBenchmarkQuery>,
        config: BenchmarkConfig,
    ) -> Result<Self> {
        let manager = PostgresConnectionManager::new_from_stringlike(db_url.as_str(), NoTls)?;
        let pool = Pool::builder().build(manager).await?;

        Ok(Self {
            pool,
            enriched_queries,
            config,
            metrics: MetricsCollector::default(),
        })
    }

    async fn worker_task(
        pool: Pool<PostgresConnectionManager<NoTls>>,
        enriched_queries: Vec<EnrichedBenchmarkQuery>,
        metrics: MetricsCollector,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let client = pool.get().await?;
        let mut rng = rand::rngs::StdRng::from_entropy();
        while deadline.is_none_or(|d| Instant::now() < d) {
            let enriched = enriched_queries
                .choose(&mut rng)
                .context("No queries available")?;
            let Some(row) = enriched.rows.choose(&mut rng) else {
                // skip when the table is empty and thus no values to sample.
                continue;
            };

            let params: Vec<Box<dyn ToSql + Sync + Send>> = row
                .iter()
                .map(|val| match val {
                    SqlValue::Text(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                    SqlValue::Int4(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                    SqlValue::Int8(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                    SqlValue::Float8(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                    SqlValue::Bool(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                    SqlValue::Int2(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                    SqlValue::Bytea(v) => Box::new(v) as Box<dyn ToSql + Sync + Send>,
                })
                .collect();
            let param_refs: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(|p| p.as_ref() as &(dyn ToSql + Sync))
                .collect();

            let query_str = enriched.query.query_template.clone();

            let start = Instant::now();
            let result = client.query(&query_str, &param_refs[..]).await;

            metrics.record_query(enriched.query.clone(), start.elapsed(), result.is_err());
        }
        Ok(())
    }

    pub async fn run(&self) -> Result<BenchmarkResult> {
        info!(
            "Running benchmark with {} concurrent clients",
            self.config.concurrency
        );

        let start = Instant::now();
        let deadline = self.config.duration.map(|duration| start + duration);
        futures::stream::iter(self.enriched_queries.clone())
            .try_for_each_spawned(self.config.concurrency, |query| {
                QueryExecutor::worker_task(
                    self.pool.clone(),
                    vec![query],
                    self.metrics.clone(),
                    deadline,
                )
            })
            .await?;

        Ok(self.metrics.generate_report())
    }
}
