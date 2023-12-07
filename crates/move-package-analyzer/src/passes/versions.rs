// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    model::{
        global_env::GlobalEnv,
        move_model::{FunctionIndex, Package, PackageIndex, StructIndex},
    },
    write_to,
};
use move_binary_format::file_format::Visibility;
use std::collections::BTreeMap;
use std::{fs::File, io::Write, path::Path};
use sui_types::base_types::ObjectID;
use tracing::error;

pub(crate) fn run(env: &GlobalEnv, output: &Path) {
    let file = &mut File::create(output.join("versions.txt"))
        .unwrap_or_else(|_| panic!("Unable to create file versions.txt in {}", output.display()));
    write_to!(file, "Package Versions",);

    //
    // Write count of package with a later version and how many upgrades
    let mut upgrades = 0;
    let versions: Vec<_> = env
        .packages
        .iter()
        .filter(|package| !package.versions.is_empty())
        .map(|package| {
            assert_eq!(package.version, 1, "Non root package {}", package.id);
            upgrades += package.versions.len();
            (package.self_idx, package.versions.clone())
        })
        .collect();
    write_to!(
        file,
        "* versioned packages: {}, upgrades: {}",
        versions.len(),
        upgrades,
    );

    //
    // Write the map from upgrade count to root packages
    let mut upgrades_count_map: BTreeMap<usize, Vec<PackageIndex>> = BTreeMap::new();
    versions.iter().for_each(|(package_idx, versions)| {
        let upgrades = upgrades_count_map.entry(versions.len()).or_default();
        upgrades.push(*package_idx);
    });
    let mut upgrade_counter = upgrades_count_map.into_iter().collect::<Vec<_>>();
    upgrade_counter.sort_by(|e1, e2| {
        if e1.1.len() != e2.1.len() {
            e1.1.len().cmp(&e2.1.len())
        } else {
            e1.0.cmp(&e2.0)
        }
    });
    write_to!(file, "==================== UPGRADES ===================");
    upgrade_counter.iter().rev().for_each(|(count, roots)| {
        let root_pacakges: Vec<_> = roots
            .iter()
            .map(|root| env.packages[*root].id.to_string())
            .collect();
        write_to!(
            file,
            "{} upgrades[{}]: {}",
            count,
            root_pacakges.len(),
            root_pacakges.join(", "),
        );
    });
    study_protocols(env, file);
}

//
// Protocols non-sense
//

fn study_protocols(env: &GlobalEnv, file: &mut File) {
    let protocols = read_protocols(env);
    write_to!(
        file,
        "================ VERSION UPGRADES ===================="
    );
    protocols.iter().for_each(|protocol| {
        write_to!(file, "== Package: {}", protocol.id);
        protocol
            .protocols
            .iter()
            .enumerate()
            .for_each(|(ver, version_protocol)| {
                write_to!(file, "V{}: {}", ver + 1, version_protocol.id);
                version_protocol.modules.iter().for_each(|module_protocol| {
                    write_to!(
                        file,
                        "  Module: {}, Public: {}, Entry: {}, Types: {}, Key Types: {}",
                        module_protocol.name,
                        module_protocol.public.len(),
                        module_protocol.entry.len(),
                        module_protocol.types.len(),
                        module_protocol.key_types.len(),
                    );
                });
            });
    });
    write_to!(
        file,
        "================ PROTOCOL UPGRADES ===================="
    );
    protocols.iter().for_each(|protocol| {
        write_to!(
            file,
            "== Package: {} [{}]",
            protocol.id,
            protocol.protocols.len()
        );
        let mut versioned_modules: BTreeMap<String, Vec<(usize, ModuleProtocol)>> = BTreeMap::new();
        protocol
            .protocols
            .iter()
            .enumerate()
            .for_each(|(ver, version_protocol)| {
                version_protocol.modules.iter().for_each(|module_protocol| {
                    let modules = versioned_modules
                        .entry(module_protocol.name.clone())
                        .or_default();
                    modules.push((ver + 1, module_protocol.clone()));
                });
            });
        versioned_modules.iter().for_each(|(name, modules)| {
            write_to!(file, "Module: {}", name);
            let mut current_public = 0;
            let mut current_entry = 0;
            let mut current_types = 0;
            let mut current_key_types = 0;
            modules.iter().for_each(|(ver, module_protocol)| {
                let mut print = false;
                let mut protocol = format!("  V{}:", ver);
                if module_protocol.public.len() > current_public {
                    protocol = format!("{} Public: {},", protocol, module_protocol.public.len());
                    current_public = module_protocol.public.len();
                    print = true;
                }
                if module_protocol.entry.len() > current_entry {
                    protocol = format!("{} Entry: {},", protocol, module_protocol.entry.len());
                    current_entry = module_protocol.entry.len();
                    print = true;
                }
                if module_protocol.types.len() > current_types {
                    protocol = format!("{} Types: {},", protocol, module_protocol.types.len());
                    current_types = module_protocol.types.len();
                    print = true;
                }
                if module_protocol.key_types.len() > current_key_types {
                    protocol = format!(
                        "{} Key Types: {},",
                        protocol,
                        module_protocol.key_types.len()
                    );
                    current_key_types = module_protocol.key_types.len();
                    print = true;
                }
                if print {
                    write_to!(file, "{}", protocol);
                }
            });
        });
    });
}

#[derive(Clone, Debug)]
struct PackageProtocol {
    id: ObjectID, // the root id
    // all versions of this package, including the root
    protocols: Vec<VersionProtocol>,
}

#[derive(Clone, Debug)]
struct VersionProtocol {
    id: ObjectID,
    modules: Vec<ModuleProtocol>,
}

#[derive(Clone, Debug)]
struct ModuleProtocol {
    name: String,                // module name must match future version
    public: Vec<FunctionIndex>,  // all public function in a module, entry or not
    entry: Vec<FunctionIndex>,   // all non public entries
    types: Vec<StructIndex>,     // all types in a module
    key_types: Vec<StructIndex>, // all key types in a module
}

impl ModuleProtocol {
    fn new(name: String) -> Self {
        Self {
            name,
            public: vec![],
            entry: vec![],
            types: vec![],
            key_types: vec![],
        }
    }
}

fn read_protocols(env: &GlobalEnv) -> Vec<PackageProtocol> {
    env.packages
        .iter()
        .filter(|package| !package.versions.is_empty())
        .map(|package| {
            assert_eq!(package.version, 1, "Non root package {}", package.id);
            let mut protocol = PackageProtocol {
                id: package.id, // root package id
                protocols: vec![],
            };
            protocol.protocols.push(read_package_protocol(env, package));
            package.versions.iter().for_each(|version| {
                let upgrade = &env.packages[*version];
                protocol.protocols.push(read_package_protocol(env, upgrade));
            });
            protocol
        })
        .collect()
}

fn read_package_protocol(env: &GlobalEnv, package: &Package) -> VersionProtocol {
    let modules = env
        .modules_in_package(package)
        .map(|module| {
            let module_name = env.module_name(module);
            let mut module_protocol = ModuleProtocol::new(module_name.clone());
            module.functions.iter().for_each(|func_idx| {
                let func = &env.functions[*func_idx];
                if func.visibility == Visibility::Public {
                    module_protocol.public.push(*func_idx);
                } else if func.is_entry {
                    module_protocol.entry.push(*func_idx);
                }
            });
            module.structs.iter().for_each(|struct_idx| {
                let struct_ = &env.structs[*struct_idx];
                if struct_.abilities.has_key() {
                    module_protocol.key_types.push(*struct_idx);
                }
                module_protocol.types.push(*struct_idx);
            });
            module_protocol
        })
        .collect();
    VersionProtocol {
        id: package.id,
        modules,
    }
}
