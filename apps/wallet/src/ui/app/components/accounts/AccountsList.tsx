// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { Filter16, Plus12 } from '@mysten/icons';

import { useState } from 'react';
import { AccountItem } from './AccountItem';

import { UnlockAccountModal } from './UnlockAccountModal';

import { useAccounts } from '../../hooks/accounts-v2/useAccounts';

import { Heading } from '../../shared/heading';
import { FooterLink } from './FooterLink';

export function AccountsList() {
	const { data: accounts } = useAccounts();
	// todo: replace this with a real flow
	const [unlockModalOpen, setUnlockModalOpen] = useState(false);

	const close = () => setUnlockModalOpen(false);

	return (
		<div className="bg-gradients-graph-cards flex flex-col rounded-xl p-4 gap-5 border border-solid border-hero/10 w-full">
			<Heading variant="heading5" weight="semibold" color="steel-darker">
				Accounts
			</Heading>

			{accounts?.map((account) => {
				return <AccountItem address={account.address} />;
			})}

			<div className="flex justify-between">
				<FooterLink color="steelDarker" icon={<Filter16 />} to="/accounts/manage" text="Manage" />
				<FooterLink color="steelDarker" icon={<Plus12 />} to="/accounts/add-account" text="Add" />
			</div>
			{unlockModalOpen ? <UnlockAccountModal onClose={close} onSuccess={close} /> : null}
		</div>
	);
}
