// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import {
	isSuiNSName,
	useGetSuiNsAvatar,
	useResolveSuiNSAddress,
	useResolveSuiNSName,
} from '@mysten/core';
import { Heading, LoadingIndicator } from '@mysten/ui';
import { useWalletKit } from '@mysten/wallet-kit';
import { useEffect } from 'react';
import { useParams } from 'react-router-dom';

import { ErrorBoundary } from '../../components/error-boundary/ErrorBoundary';
import { TransactionsForAddress } from '../../components/transactions/TransactionsForAddress';
import { PageLayout } from '~/components/Layout/PageLayout';
import { OwnedCoins } from '~/components/OwnedCoins';
import { OwnedObjects } from '~/components/OwnedObjects';
import { Link } from '~/ui/Link';
import { PageHeader } from '~/ui/PageHeader';
import { Image } from '~/ui/image/Image';

export function SuiNsAvatar({ address }: { address?: string }) {
	const { data: domainName } = useResolveSuiNSName(address);
	const avatar = useGetSuiNsAvatar(domainName!);
	const { wallets, currentAccount, connect } = useWalletKit();
	const addressIsUser = currentAccount?.address === address;

	useEffect(() => {
		connect(wallets[0].name);
	}, [wallets, connect]);

	if (!address || !avatar || !avatar.data) return null;

	return addressIsUser ? (
		<Link
			href="https://suins.io/account/my-names"
			className={`${addressIsUser ? 'cursor-pointer' : ''}`}
		>
			<Image src={avatar.data?.image_url} alt={domainName ?? address} size="md" rounded="2xl" />
		</Link>
	) : (
		<Image src={avatar.data?.image_url} alt={domainName ?? address} size="md" rounded="2xl" />
	);
}

function AddressResult({ address }: { address: string }) {
	return (
		<div className="space-y-12">
			<div>
				<div className="border-b border-gray-45 pb-5 md:mt-12">
					<Heading color="gray-90" variant="heading4/semibold">
						Owned Objects
					</Heading>
				</div>
				<ErrorBoundary>
					<div className="flex flex-col gap-10 md:flex-row">
						<div className="flex-1 overflow-hidden">
							<OwnedCoins id={address} />
						</div>
						<div className="hidden w-px bg-gray-45 md:block" />
						<div className="flex-1 overflow-hidden">
							<OwnedObjects id={address} />
						</div>
					</div>
				</ErrorBoundary>
			</div>

			<div>
				<ErrorBoundary>
					<div className="mt-2">
						<TransactionsForAddress address={address} type="address" />
					</div>
				</ErrorBoundary>
			</div>
		</div>
	);
}

function SuiNSAddressResult({ name }: { name: string }) {
	const { isFetched, data } = useResolveSuiNSAddress(name);

	if (!isFetched) {
		return <LoadingIndicator />;
	}

	// Fall back into just trying to load the name as an address anyway:
	return <AddressResult address={data ?? name} />;
}

export default function AddressResultPage() {
	const { id: address } = useParams();
	const { data: domainName } = useResolveSuiNSName(address);

	return (
		<PageLayout
			gradientContent={
				<PageHeader
					before={
						<div className="rounded-xl">
							<SuiNsAvatar address={address} />
						</div>
					}
					type="Address"
					title={address!}
					subtitle={domainName}
				/>
			}
			content={
				isSuiNSName(address!) ? (
					<SuiNSAddressResult name={address!} />
				) : (
					<AddressResult address={address!} />
				)
			}
		/>
	);
}
