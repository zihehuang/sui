// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
import { SuinsClient } from '@mysten/suins-toolkit';
import { useRpcClient } from '../api/RpcClientContext';
import { useEffect, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useGetObject } from './useGetObject';
import { getObjectDisplay } from '@mysten/sui.js';

export function useSuiNsClient() {
	const rpc = useRpcClient();
	const [client, setClient] = useState<SuinsClient | null>(null);

	useEffect(() => {
		const suiNsClient = new SuinsClient(rpc, {
			contractObjects: {
				packageId: '0xd22b24490e0bae52676651b4f56660a5ff8022a2576e0089f79b3c88d44e08f0',
				registry: '0xe64cd9db9f829c6cc405d9790bd71567ae07259855f4fba6f02c84f52298c106',
				reverseRegistry: '0x2fd099e17a292d2bc541df474f9fafa595653848cbabb2d7a4656ec786a1969f',
				suins: '0x6e0ddefc0ad98889c04bab9639e512c21766c5e6366f89e696956d9be6952871',
			},
		});
		setClient(suiNsClient);
	}, []);

	return client;
}

export function useGetDomainObject(domainName?: string) {
	const suiNsClient = useSuiNsClient();

	return useQuery({
		// eslint-disable-next-line @tanstack/query/exhaustive-deps
		queryKey: ['suins-domain', domainName],
		queryFn: async () => {
			if (!domainName) return null;
			const data = await suiNsClient!.getNameObject(domainName);
			console.log(data);
			return data;
		},
		enabled: !!suiNsClient,
	});
}

export function useGetSuiNsAvatar(domainName?: string) {
	const { data: domainObject } = useGetDomainObject(domainName);
	const { data: objectData } = useGetObject(domainObject?.avatar);
	if (!objectData) return null;

	return getObjectDisplay(objectData);
}
