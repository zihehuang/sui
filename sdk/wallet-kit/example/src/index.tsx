// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import React from 'react';
import ReactDOM from 'react-dom/client';

import './index.css';

import { WalletKitProvider } from '@mysten/wallet-kit';

import App from './App';

export const root = ReactDOM.createRoot(document.getElementById('root') as HTMLElement);
root.render(
	<React.StrictMode>
		<WalletKitProvider features={['sui:signTransactionBlock']} enableUnsafeBurner>
			<App />
		</WalletKitProvider>
	</React.StrictMode>,
);