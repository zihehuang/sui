// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::model::{
    global_env::GlobalEnv,
    move_model::{Bytecode, Function},
};

pub fn walk_bytecodes<F>(env: &GlobalEnv, mut walker: F)
where
    F: FnMut(&GlobalEnv, &Function, &Bytecode),
{
    env.functions.iter().for_each(|func| {
        if let Some(code) = func.code.as_ref() {
            code.code
                .iter()
                .for_each(|bytecode| walker(env, func, bytecode));
        }
    });
}
