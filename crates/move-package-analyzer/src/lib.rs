// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::errors::PackageAnalyzerError;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::{
    collections::HashSet,
    {fs, path::Path},
};
use sui_types::base_types::ObjectID;

pub mod errors;
pub mod load_from_dir;
pub mod model;
pub mod passes;
pub mod query_indexer;

// Global constants
const DEFAULT_CAPACITY: usize = 16 * 1024;
const PACKAGE_BCS: &str = "package.bcs";

pub static FRAMEWORK: Lazy<HashSet<ObjectID>> = Lazy::new(|| {
    let mut framework = HashSet::new();
    framework.insert(
        ObjectID::from_hex_literal(
            "0x0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap(),
    );
    framework.insert(
        ObjectID::from_hex_literal(
            "0x0000000000000000000000000000000000000000000000000000000000000002",
        )
        .unwrap(),
    );
    framework.insert(
        ObjectID::from_hex_literal(
            "0x0000000000000000000000000000000000000000000000000000000000000003",
        )
        .unwrap(),
    );
    framework.insert(
        ObjectID::from_hex_literal(
            "0x000000000000000000000000000000000000000000000000000000000000dee9",
        )
        .unwrap(),
    );
    framework
});

/// Passes available.
/// Annoyance: when adding a pass one has to come here, added and add it in
/// `pass_manager.rs` as well. It can then be called from `passes.yaml`.
/// We may review how a pass is exported but for now we'll survive.
#[derive(Debug, Deserialize)]
pub enum Pass {
    /// No pass, just to have something in the `passes.yaml` file.
    Noop,
    /// Write summary information of the environment in a `summary.txt` file.
    /// Aggregate information of all packages.
    Summary,
    /// Write out the environment in different formats that can be viewed and
    /// make sense of how a package is composed.
    DumpEnv,
    /// Write out `csv` files for all expected vm/language entyties in the system:
    /// packages, modules, functions, structs, ...
    CsvEntities,
    /// Report (`versions.txt`) version information for upgrades packages.
    Versions,
}

#[derive(Debug, Deserialize)]
pub struct PassesConfig {
    pub passes: Vec<Pass>,
    pub output_dir: Option<String>,
}

pub fn load_config(path: &Path) -> Result<PassesConfig, PackageAnalyzerError> {
    let reader = fs::File::open(path).map_err(|e| {
        PackageAnalyzerError::BadConfig(format!(
            "Cannot open config file {}: {}",
            path.display(),
            e
        ))
    })?;
    let config: PassesConfig = serde_yaml::from_reader(reader).map_err(|e| {
        PackageAnalyzerError::BadConfig(format!(
            "Cannot parse config file {}: {}",
            path.display(),
            e
        ))
    })?;
    Ok(config)
}

#[macro_export]
macro_rules! write_to {
    ($file:expr, $($arg:tt)*) => {{
        writeln!($file, $($arg)*).unwrap_or_else(|e| error!(
            "Unable to write to file: {}",
            e.to_string()
        ))
    }};
}
