// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
import { SUI_TYPE_ARG } from '@mysten/sui.js/utils';
import { useMutation, useQuery } from '@tanstack/react-query';
import { Aftermath } from 'aftermath-ts-sdk';
import { useNavigate } from 'react-router-dom';

import { parseAmount } from '../../helpers';
import { useActiveAccount } from '../../hooks/useActiveAccount';
import { useSigner } from '../../hooks/useSigner';

export function useAftermathSdk() {
	// TODO: Use correct network
	const aftermath = new Aftermath('MAINNET');

	return {
		router: aftermath.Router(),
		pools: aftermath.Pools(),
		prices: aftermath.Prices(),
		staking: aftermath.Staking(),
	};
}

export function useGetAfSuiExchangeRate() {
	const aftermath = useAftermathSdk();

	return useQuery({
		queryKey: ['afsui-exchange-rate'],
		queryFn: () => aftermath.staking.getAfSuiToSuiExchangeRate(),
	});
}

export function useSwapMutation() {
	const aftermath = useAftermathSdk();
	const account = useActiveAccount();
	const signer = useSigner(account);

	return useMutation({
		mutationKey: ['aftermath-swap'],
		mutationFn: async ({
			amount,
			coinInType = SUI_TYPE_ARG,
			coinOutType = '0x5d4b302506645c37ff133b98c4b50a5ae14841659738d6d733d59d0d217a93bf::coin::COIN',
			slippage = 0.1,
		}: {
			amount: string;
			dryRun?: boolean;
			coinInType?: string;
			coinOutType?: string;
			slippage: number;
		}) => {
			const route = await aftermath.router.getCompleteTradeRouteGivenAmountIn({
				coinInAmount: parseAmount(amount, 9),
				coinInType,
				coinOutType,
			});

			const tx = await aftermath.router.getTransactionForCompleteTradeRoute({
				walletAddress: account?.address!,
				completeRoute: route,
				slippage,
			});

			return signer?.signAndExecuteTransactionBlock({ transactionBlock: tx });
		},
	});
}

// export function useTradeAmountOut() {
// 	const { pools } = useAftermathSdk();

// 	return useQuery({
// 		queryKey: ['trade-amount-out'],
// 		queryFn: async (coinType: any) => {
// 			const [pool] = await pools.getAllPools();
// 			pool.getTradeAmountOut({
// 				coinInType: '',
// 				coinInAmount: '',
// 				coinOutType: '',
// 			});
// 		},
// 	});
// }

export function useSupportedCoins() {
	const aftermath = useAftermathSdk();

	return useQuery({
		queryKey: ['aftermath-supported-coins'],
		queryFn: () => aftermath.router.getSupportedCoins(),
	});
}

function useLiquidStakeMutation() {
	const aftermath = useAftermathSdk();
	const account = useActiveAccount();
	const navigate = useNavigate();
	const signer = useSigner(account);
	return useMutation({
		mutationKey: ['aftermath-liquid-stake'],
		mutationFn: async ({
			amount,
			validatorAddress,
		}: {
			amount: string;
			validatorAddress: string;
		}) => {
			const tx = await aftermath.staking.getStakeTransaction({
				walletAddress: account?.address!,
				validatorAddress: '0xd30018ec3f5ff1a3c75656abf927a87d7f0529e6dc89c7ddd1bd27ecb05e3db2',
				suiStakeAmount: parseAmount(amount, 9),
			});

			return signer?.signAndExecuteTransactionBlock({ transactionBlock: tx });
		},
		onSuccess: (tx) => {
			const receiptUrl = `/receipt?txdigest=${encodeURIComponent(tx?.digest!)}&from=transactions`;
			return navigate(receiptUrl);
		},
	});
}

export function useGetStakingPositions() {
	const aftermath = useAftermathSdk();
	const account = useActiveAccount();

	return useQuery({
		queryKey: ['staking-positions', account?.address],
		queryFn: async () => {
			if (!account?.address) return;
			return aftermath.staking.getStakingPositions({ walletAddress: account?.address });
		},
		enabled: !!account?.address,
	});
}

function useLiquidUnstakeMutation() {
	const aftermath = useAftermathSdk();
	const account = useActiveAccount();
	const navigate = useNavigate();
	const signer = useSigner(account);
	return useMutation({
		mutationKey: ['aftermath-liquid-unstake'],
		mutationFn: async ({ amount }: { amount: string }) => {
			const tx = await aftermath.staking.getUnstakeTransaction({
				walletAddress: account?.address!,
				isAtomic: true,
				afSuiUnstakeAmount: parseAmount(amount, 9),
			});

			return signer?.signAndExecuteTransactionBlock({ transactionBlock: tx });
		},
		onSuccess: (tx) => {
			const receiptUrl = `/receipt?txdigest=${encodeURIComponent(tx?.digest!)}&from=transactions`;
			return navigate(receiptUrl);
		},
	});
}

export function useLiquidStaking() {
	return {
		stake: useLiquidStakeMutation(),
		unstake: useLiquidUnstakeMutation(),
		afSuiExchangeRate: useGetAfSuiExchangeRate(),
		stakingPositions: useGetStakingPositions(),
	};
}
