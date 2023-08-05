// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { type ReactNode, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { UnlockAccountModal } from '../components/accounts/UnlockAccountModal';

export function PasswordProtected({ children }: { children: ReactNode }) {
	const [isAuthenticated, setIsAuthenticated] = useState(false);
	const navigate = useNavigate();

	return isAuthenticated ? (
		children
	) : (
		<UnlockAccountModal
			onClose={() => {
				navigate(-1);
			}}
			onSuccess={() => setIsAuthenticated(true)}
		/>
	);
}
