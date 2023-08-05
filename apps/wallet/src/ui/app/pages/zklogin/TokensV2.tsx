// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import TokenDetails from '../home/tokens/TokensDetails';

export function TokensV2() {
	return (
		<div className="flex flex-col gap-4">
			<TokenDetails />
		</div>
	);
}
