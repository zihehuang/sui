// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    model::{global_env::GlobalEnv, move_model::Bytecode},
    write_to, FRAMEWORK,
};
use move_binary_format::file_format::Visibility;
use std::collections::BTreeSet;
use std::{fs::File, io::Write, path::Path};
use tracing::error;

pub(crate) fn run(env: &GlobalEnv, output: &Path) {
    packages(env, output);
    modules(env, output);
    binary_modules(env, output);
    functions(env, output);
    structs(env, output);
}

/// Generate a `packages.csv` file with package information.
fn packages(env: &GlobalEnv, output: &Path) {
    let file = &mut File::create(output.join("packages.csv"))
        .unwrap_or_else(|_| panic!("Unable to create file packages.csv in {}", output.display()));

    write_to!(
        file,
        "package_id, version, root_version, \
        dependencies, versioned_dependencies, direct_dependencies, indirect_dependencies, origin_tables, \
        modules, structs, functions, constants, \
        public_functions, entries, non_public_entries, \
        internal_calls, cross_package_calls, framework_calls"
    );

    env.packages.iter().for_each(|package| {
        let dependencies: BTreeSet<_> = package
            .dependencies
            .keys()
            .cloned()
            .chain(package.dependencies.values().cloned())
            .collect();
        let mut struct_count = 0;
        let mut func_count = 0;
        let mut const_count = 0;
        let mut public = 0;
        let mut entries = 0;
        let mut non_public_entries = 0;
        let mut internal_calls = 0;
        let mut cross_package_calls = 0;
        let mut framework_calls = 0;
        env.modules_in_package(package).for_each(|module| {
            struct_count += module.structs.len();
            func_count += module.functions.len();
            const_count += module.constants.len();
            for func_idx in module.functions.iter() {
                let func = &env.functions[*func_idx];
                if func.visibility == Visibility::Public {
                    public += 1;
                    if func.is_entry {
                        entries += 1;
                    }
                } else if func.is_entry {
                    non_public_entries += 1;
                }
                if let Some(code) = func.code.as_ref() {
                    code.code.iter().for_each(|bytecode| match bytecode {
                        Bytecode::Call(func_idx) | Bytecode::CallGeneric(func_idx, _) => {
                            let func = &env.functions[*func_idx];
                            let func_pkg = func.package;
                            if FRAMEWORK.contains(&env.packages[func_pkg].id) {
                                framework_calls += 1;
                            } else if dependencies.contains(&func_pkg) {
                                cross_package_calls += 1;
                            } else {
                                internal_calls += 1;
                            }
                        }
                        _ => (),
                    });
                };
            }
        });
        write_to!(
            file,
            "{}, {}, {}, \
            {}, {}, {}, {}, {},\
            {}, {}, {}, {}, \
            {}, {}, {}, \
            {}, {}, {}",
            // package_id, version, root_version,
            package.id,
            package.version,
            env.packages[package.root_version.unwrap_or(package.self_idx)].id,
            // dependencies, versioned_dependencies, direct_dependencies, indirect_dependencies, origin_tables
            package.dependencies.len(),
            package
                .dependencies
                .iter()
                .map(|(origin, dest)| { origin != dest })
                .filter(|&is_versioned| is_versioned)
                .count(),
            package.direct_dependencies.len(),
            package
                .dependencies
                .values()
                .cloned()
                .collect::<BTreeSet<_>>()
                .difference(&package.direct_dependencies)
                .count(),
            package.type_origin.len(),
            // modules, structs, functions, constants
            package.modules.len(),
            struct_count,
            func_count,
            const_count,
            // public_functions, entries, non_public_entries
            public,
            entries,
            non_public_entries,
            // internal_calls, cross_package_calls
            internal_calls,
            cross_package_calls,
            framework_calls,
        );
    });
}

/// Generate a `modules.csv` file with module information.
fn modules(env: &GlobalEnv, output: &Path) {
    let file = &mut File::create(output.join("modules.csv"))
        .unwrap_or_else(|_| panic!("Unable to create file modules.csv in {}", output.display()));
    write_to!(
        file,
        "package, version, module, \
        structs, functions, constants, \
        public_functions, entries, non_public_entries"
    );
    env.modules.iter().for_each(|module| {
        let struct_count = module.structs.len();
        let func_count = module.functions.len();
        let const_count = module.constants.len();
        let mut public = 0;
        let mut entries = 0;
        let mut non_public_entries = 0;
        for func_idx in module.functions.iter() {
            let func = &env.functions[*func_idx];
            if func.visibility == Visibility::Public {
                public += 1;
                if func.is_entry {
                    entries += 1;
                }
            } else if func.is_entry {
                non_public_entries += 1;
            }
        }
        let package = &env.packages[module.package];
        write_to!(
            file,
            "{}, {}, 0x{}, {}, {}, {}, {}, {}, {}",
            package.id,
            package.version,
            module.module_id,
            struct_count,
            func_count,
            const_count,
            public,
            entries,
            non_public_entries,
        );
    });
}

/// Generate a `binary_modules.csv` file with compiled module information.
fn binary_modules(env: &GlobalEnv, output: &Path) {
    let file = &mut File::create(output.join("binary_modules.csv")).unwrap_or_else(|_| {
        panic!(
            "Unable to create file bynary_module.csv in {}",
            output.display()
        )
    });
    write_to!(
        file,
        "package, version, module, \
        module_handles, struct_handles, function_handles, field_handles, \
        struct_def_instantiations, function_instantiations, field_instantiations, \
        signatures, identifiers, address_identifiers, constant_pool, \
        struct_defs, function_defs"
    );
    env.modules.iter().for_each(|module| {
        if let Some(compiled_module) = module.module.as_ref() {
            let package = &env.packages[module.package];
            write_to!(
                file,
                "{}, {}, 0x{}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}",
                package.id,
                package.version,
                module.module_id,
                compiled_module.module_handles.len(),
                compiled_module.struct_handles.len(),
                compiled_module.function_handles.len(),
                compiled_module.field_handles.len(),
                compiled_module.struct_def_instantiations.len(),
                compiled_module.function_instantiations.len(),
                compiled_module.field_instantiations.len(),
                compiled_module.signatures.len(),
                compiled_module.identifiers.len(),
                compiled_module.address_identifiers.len(),
                compiled_module.constant_pool.len(),
                compiled_module.struct_defs.len(),
                compiled_module.function_defs.len(),
            );
        }
    });
}

/// Generate a `functions.csv` file with function information.
fn functions(env: &GlobalEnv, output: &Path) {
    let file = &mut File::create(output.join("functions.csv")).unwrap_or_else(|_| {
        panic!(
            "Unable to create file functions.csv in {}",
            output.display()
        )
    });
    write_to!(
        file,
        "package, version, module_address, module, function, visibility, entry, \
            type_parameters, parameters, returns, \
            instructions, package_calls, cross_package_calls"
    );
    env.functions.iter().for_each(|func| {
        let package = &env.packages[func.package];
        let module = &env.modules[func.module];
        let mut in_package = 0usize;
        let mut cross_package = 0usize;
        let code_len = func.code.as_ref().map_or(0, |code| {
            code.code.iter().for_each(|bytecode| match bytecode {
                Bytecode::Call(func_idx) | Bytecode::CallGeneric(func_idx, _) => {
                    let callee = &env.functions[*func_idx];
                    if callee.package == func.package {
                        in_package += 1;
                    } else {
                        cross_package += 1;
                    }
                }
                _ => {}
            });
            code.code.len()
        });
        write_to!(
            file,
            "{}, {}, 0x{}, {}, {}, {:?}, {}, {}, {}, {}, {}, {}, {}",
            package.id,
            package.version,
            module.module_id.address(),
            env.module_name(module),
            env.function_name(func),
            func.visibility,
            func.is_entry,
            func.type_parameters.len(),
            func.parameters.len(),
            func.returns.len(),
            code_len,
            in_package,
            cross_package,
        );
    })
}

/// Generate a `structs.csv` file with type information.
fn structs(env: &GlobalEnv, output: &Path) {
    let file = &mut File::create(output.join("structs.csv"))
        .unwrap_or_else(|_| panic!("Unable to create file structs.csv in {}", output.display()));
    write_to!(
        file,
        "package, version, module_address, module, struct, type_parameters, abilities, fields"
    );
    env.structs.iter().for_each(|struct_| {
        let package = &env.packages[struct_.package];
        let module = &env.modules[struct_.module];
        write_to!(
            file,
            "{}, {}, 0x{}, {}, {}, {}, {:04b}, {}",
            package.id,
            package.version,
            module.module_id.address(),
            env.module_name(module),
            env.struct_name(struct_),
            struct_.type_parameters.len(),
            struct_.abilities.into_u8(),
            struct_.fields.len()
        );
    })
}
