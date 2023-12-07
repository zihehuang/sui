// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{errors::PackageAnalyzerError, DEFAULT_CAPACITY, PACKAGE_BCS};
use std::{fs, fs::File, io::Read, path::PathBuf};
use sui_types::move_package::MovePackage;
use tracing::info;

pub fn load_from_directory(path: PathBuf) -> Result<Vec<MovePackage>, PackageAnalyzerError> {
    let entries = fs::read_dir(path.clone()).map_err(|_| {
        PackageAnalyzerError::BadDirectoryStructure(format!("Cannot read path {}", path.display()))
    })?;
    let mut packages = Vec::with_capacity(DEFAULT_CAPACITY);
    for entry in entries {
        if let Ok(package_entry) = entry {
            let package_path = package_entry.path().join(PACKAGE_BCS);
            let mut file = File::open(package_path.clone()).map_err(|e| {
                PackageAnalyzerError::BadDirectoryStructure(format!(
                    "Cannot open file {}: {:?}",
                    package_path.display(),
                    e
                ))
            })?;
            let mut bytes = vec![];
            file.read_to_end(&mut bytes).map_err(|e| {
                PackageAnalyzerError::BadDirectoryStructure(format!(
                    "Cannot read file {}: {:?}",
                    package_path.display(),
                    e
                ))
            })?;
            let move_package = bcs::from_bytes::<MovePackage>(&bytes).map_err(|e| {
                PackageAnalyzerError::BadDirectoryStructure(format!(
                    "Cannot deserialize package {}: {:?}",
                    package_path.display(),
                    e
                ))
            })?;
            packages.push(move_package);
        } else {
            info!("bad DirEntry {:?} - continue...", entry);
        }
    }
    Ok(packages)
}
