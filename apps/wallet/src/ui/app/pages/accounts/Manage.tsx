// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { Plus12 } from '@mysten/icons';
import { useNavigate } from 'react-router-dom';
import { AccountItem } from '../../components/accounts/AccountItem';
import { ButtonLink } from '../../components/accounts/ButtonLink';
import { AccountActions } from '../../components/menu/content/AccountActions';
import Overlay from '../../components/overlay';
import { useAccounts } from '../../hooks/useAccounts';
import { Heading } from '../../shared/heading';

export function ManageAccountsPage() {
	const accounts = useAccounts();
	const navigate = useNavigate();
	return (
		<Overlay showModal title="Manage Accounts" closeOverlay={() => navigate('/home')}>
			<div className="flex flex-col gap-4">
				<div className="flex gap-2 w-full items-center justify-center">
					<Heading variant="heading5" weight="semibold" color="steel-darker">
						Passphrase Derived
					</Heading>
					<div className="h-px bg-gray-45 flex flex-1" />

					<ButtonLink color="heroOp40" icon={<Plus12 />} text="Create New" />
				</div>
				{accounts.map((account) => {
					return (
						<div className="flex flex-col gap-3">
							<AccountItem
								gradient
								address={account.address}
								after={
									<div className="flex flex-shrink-0 w-full">
										<div className="flex gap-3">
											<div className="w-4" />
											<ButtonLink color="heroOp40" to="/edit" text="Edit Nickname" />
											<ButtonLink color="heroOp40" to="/remove" text="Remove" />
										</div>
									</div>
								}
							/>
							<AccountActions account={account} />
						</div>
					);
				})}
			</div>
		</Overlay>
	);
}
