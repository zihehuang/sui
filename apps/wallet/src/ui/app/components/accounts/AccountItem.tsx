// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { useResolveSuiNSName } from '@mysten/core';
import { ArrowUpRight12, Copy12 } from '@mysten/icons';
import { formatAddress } from '@mysten/sui.js/utils';

import cn from 'classnames';
import { forwardRef, type ReactNode } from 'react';
import { useAccounts } from '../../hooks/useAccounts';
import { useCopyToClipboard } from '../../hooks/useCopyToClipboard';
import { useExplorerLink } from '../../hooks/useExplorerLink';
import { IconButton } from '../IconButton';
import { ExplorerLinkType } from '../explorer-link/ExplorerLinkType';
import { Text } from '_src/ui/app/shared/text';

interface AccountItemProps {
	name?: string;
	address: string;
	icon?: ReactNode;
	after?: ReactNode;
	disabled?: boolean;
	gradient?: boolean;
	selected?: boolean;
	// todo: extract into variants if possible
	background?: 'gradient';
}

export const AccountItem = forwardRef<HTMLDivElement, AccountItemProps>(
	({ background, selected, disabled, icon, name, address, after, ...props }, ref) => {
		const { data: accounts } = useAccounts();
		const { data: domainName } = useResolveSuiNSName(address);
		const account = accounts?.find((account) => account.address === address);
		const accountName = account?.nickname ?? domainName ?? formatAddress(address);
		const copyAddress = useCopyToClipboard(account?.address!, {
			copySuccessMessage: 'Address copied',
		});
		const explorerHref = useExplorerLink({
			type: ExplorerLinkType.address,
			address: account?.address!,
		});

		if (!account) return null;

		return (
			<div
				ref={ref}
				className={cn(
					'flex flex-wrap items-center gap-3 px-4 py-3 rounded-xl border border-solid border-hero/10 bg-white/40 hover:bg-white/80 group',
					{ 'bg-white/80 shadow-card-soft': selected },
					{ 'bg-hero/10 border-none hover:bg-white/40 shadow-none': disabled },
					{ 'bg-gradients-graph-cards': background === 'gradient' },
				)}
				{...props}
			>
				{icon}
				<div className="flex flex-col gap-1 overflow-hidden items-start">
					<Text variant="pBody" weight="semibold" color="steel-darker" truncate>
						{accountName}
					</Text>
					<div className="text-steel-dark flex gap-1.5 leading-none">
						<Text variant="subtitle" weight="semibold" truncate>
							{formatAddress(account.address)}
						</Text>
						<IconButton icon={<Copy12 />} onClick={copyAddress} variant="subtle" />
						{explorerHref ? (
							<IconButton title="View on Explorer" href={explorerHref} icon={<ArrowUpRight12 />} />
						) : null}
					</div>
				</div>
				{after}
			</div>
		);
	},
);
