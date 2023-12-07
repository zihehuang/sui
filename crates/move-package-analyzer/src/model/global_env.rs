// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::model::move_model::{
    Field, Function, FunctionIndex, IdentifierIndex, Module, ModuleIndex, Package, PackageIndex,
    Struct, StructIndex,
};
use std::collections::{BTreeMap, BTreeSet};
use sui_types::base_types::ObjectID;

#[derive(Debug)]
pub struct GlobalEnv {
    // All entities are based on the ObjectID of the package the entity
    // lives in. There is no reconciliation of modules, types or functions
    // as related to versioning. In other words a module, a type or a function
    // is "repeated" across versions, even if the entity is unchanged.
    // There are multiple copies of an entities across packages.
    // However because the entity is based on the package ObjectID,
    // is like if they were all different entities.
    // Example:
    // package 0xf001 { module m { struct A {}}}
    // and later a new version of the package is deployed
    // package 0xf002 { module m { struct A {}}}
    // The `structs` vector and the `struct_map` map will have two entries
    // for `struct A`, one for each package.

    //
    // Pools of Move "entities" across packages.
    // All entries are unique. Everything is interned.
    //
    pub packages: Vec<Package>,
    pub modules: Vec<Module>,
    pub functions: Vec<Function>,
    pub structs: Vec<Struct>,
    pub identifiers: Vec<String>,

    //
    // maps of Move "entities" across packages
    //

    // key: <package_id>
    pub package_map: BTreeMap<ObjectID, PackageIndex>,
    // key: <package_id>::<module_name>
    pub module_map: BTreeMap<String, ModuleIndex>,
    // key: <package_id>::<module_name>::<function_name>
    pub function_map: BTreeMap<String, FunctionIndex>,
    // key: <package_id>::<module_name>::<struct_name>
    pub struct_map: BTreeMap<String, StructIndex>,

    // identifiers
    pub identifier_map: BTreeMap<String, IdentifierIndex>,

    //
    // Pre-computed, static call graphs
    //
    pub call_graph: BTreeMap<FunctionIndex, BTreeSet<FunctionIndex>>,
    pub reverse_call_graph: BTreeMap<FunctionIndex, BTreeSet<FunctionIndex>>,

    // Framework
    pub framework: BTreeMap<PackageIndex, ObjectID>,
}

impl GlobalEnv {
    pub fn module_name_from_idx(&self, idx: ModuleIndex) -> String {
        let module = &self.modules[idx];
        self.module_name(module)
    }

    pub fn module_name(&self, module: &Module) -> String {
        let name = module.name;
        self.identifiers[name].clone()
    }

    pub fn struct_name_from_idx(&self, idx: StructIndex) -> String {
        let struct_ = &self.structs[idx];
        self.struct_name(struct_)
    }

    pub fn struct_name(&self, struct_: &Struct) -> String {
        let name = struct_.name;
        self.identifiers[name].clone()
    }

    pub fn function_name_from_idx(&self, idx: FunctionIndex) -> String {
        let func = &self.functions[idx];
        self.function_name(func)
    }

    pub fn function_name(&self, func: &Function) -> String {
        let name = func.name;
        self.identifiers[name].clone()
    }

    pub fn field_name(&self, field: &Field) -> String {
        let name = field.name;
        self.identifiers[name].clone()
    }

    pub fn modules_in_package<'a>(
        &'a self,
        package: &'a Package,
    ) -> impl Iterator<Item = &Module> + 'a {
        package
            .modules
            .iter()
            .map(move |module_idx| &self.modules[*module_idx])
    }

    pub fn functions_in_package<'a>(
        &'a self,
        package: &'a Package,
    ) -> impl Iterator<Item = &Function> + 'a {
        package
            .modules
            .iter()
            .map(move |module_idx| &self.modules[*module_idx])
            .flat_map(move |module| {
                module
                    .functions
                    .iter()
                    .map(move |func_idx| &self.functions[*func_idx])
            })
    }
}
