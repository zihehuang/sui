// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    model::global_env::GlobalEnv,
    passes::{csv_entities, dump_env, summary, versions},
    Pass, PassesConfig,
};
use std::{env, path::Path, time::Instant};
use tracing::info;

pub fn run(passes: &PassesConfig, env: &GlobalEnv) {
    let output_path = if let Some(path) = passes.output_dir.as_ref() {
        Path::new(path).to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| panic!("Cannot get current directory: {}", e))
            .unwrap()
    };
    passes.passes.iter().for_each(|pass| {
        let pass_time_start = Instant::now();
        match pass {
            Pass::Noop => (),
            Pass::DumpEnv => dump_env::run(env, &output_path),
            Pass::CsvEntities => csv_entities::run(env, &output_path),
            Pass::Summary => summary::run(env, &output_path),
            Pass::Versions => versions::run(env, &output_path),
        }
        let pass_time_end = Instant::now();
        info!(
            "Run {:?} pass in {}ms",
            pass,
            pass_time_end.duration_since(pass_time_start).as_millis(),
        );
    });
}
