// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
import { useDeepBookConfigs } from '_app/hooks/deepbook/useDeepBookConfigs';
import { useDeepBookContext } from '_shared/deepBook/context';
import { SUI_TYPE_ARG } from '@mysten/sui.js/utils';

export function useRecognizedCoins() {
	const coinsMap = useDeepBookContext().configs.coinsMap;
	return Object.values(coinsMap);
}

export function useAllowedSwapCoinsList() {
	const deepBookConfigs = useDeepBookConfigs();
	const coinsMap = deepBookConfigs.coinsMap;

	return [
		SUI_TYPE_ARG,
		coinsMap.SUI,
		coinsMap.USDC,
		'0x76cb819b01abed502bee8a702b4c2d547532c12f25001c9dea795a5e631c26f1::fud::FUD',
		'0x5d4b302506645c37ff133b98c4b50a5ae14841659738d6d733d59d0d217a93bf::coin::COIN',
		'0xde2d3e02ba60b806f81ee9220be2a34932a513fe8d7f553167649e95de21c066::reap_token::REAP_TOKEN',
	];
}
