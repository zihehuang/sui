// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::{fs::File, io::Write};

pub fn generate_html_from_json(data: &str, output_path: PathBuf, network: &str) {
    let styles = include_str!("../frontend/sources/styles.css");
    let file = include_str!("../frontend/sources/index.html");

    let fixed_html = file
        .replace( "CLIENT_ENV_NETWORK", network)
        .replace("REPLACE_ME_WITH_THE_JSON_DATA_FROM_THE_REPLAY", data)
        .replace(".INSERT_STYLES_HERE{color:sui}", styles)
        // Replace the network to the one it was executed, OR the RPC endpoint.
        // .replace("network: 'mainnet',", "network: 'https://suifrens-rpc.mainnet.sui.io',")
        ;

    let mut file = File::create(output_path.clone()).ok().unwrap();
    let _ = file.write_all(fixed_html.as_bytes());
    open::that(output_path).ok();
}
