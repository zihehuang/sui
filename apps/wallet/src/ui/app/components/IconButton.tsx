// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import { type VariantProps, cva } from 'class-variance-authority';
import { type MouseEventHandler } from 'react';

interface IconButtonProps extends VariantProps<typeof buttonStyles> {
	onClick: MouseEventHandler;
	icon: JSX.Element;
}

const buttonStyles = cva(
	[
		'flex items-center rounded-sm bg-transparent border-0 p-0 text-steel-dark hover:text-hero cursor-pointer',
	],
	{
		variants: {
			variant: {
				transparent: '',
				subtle: 'hover:bg-hero-darkest/10',
			},
		},
		defaultVariants: {
			variant: 'subtle',
		},
	},
);

export function IconButton({ onClick, icon, variant }: IconButtonProps) {
	return <button onClick={onClick} className={buttonStyles({ variant })} children={icon} />;
}
