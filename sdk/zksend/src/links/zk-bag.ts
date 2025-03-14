// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import type {
	TransactionArgument,
	TransactionBlock,
	TransactionObjectArgument,
} from '@mysten/sui.js/transactions';

export interface ZkBagContractOptions {
	packageId: string;
	bagStoreId: string;
	bagStoreTableId: string;
}

export class ZkBag<IDs> {
	#package: string;
	#module = 'zk_bag' as const;
	ids: IDs;

	constructor(packageAddress: string, ids: IDs) {
		this.#package = packageAddress;
		this.ids = ids;
	}

	new(
		txb: TransactionBlock,
		{
			arguments: [store, receiver],
		}: {
			arguments: [
				store: TransactionObjectArgument | string,
				receiver: TransactionArgument | string,
			];
		},
	) {
		txb.moveCall({
			target: `${this.#package}::${this.#module}::new`,
			arguments: [
				txb.object(store),
				typeof receiver === 'string' ? txb.pure.address(receiver) : receiver,
			],
		});
	}

	add(
		txb: TransactionBlock,
		{
			arguments: [store, receiver, item],
			typeArguments,
		}: {
			arguments: [
				store: TransactionObjectArgument | string,
				receiver: TransactionArgument | string,
				item: TransactionObjectArgument | string,
			];
			typeArguments: [string];
		},
	): Extract<TransactionArgument, { kind: 'Result' }> {
		return txb.moveCall({
			target: `${this.#package}::${this.#module}::add`,
			arguments: [
				txb.object(store),
				typeof receiver === 'string' ? txb.pure.address(receiver) : receiver,
				txb.object(item),
			],
			typeArguments: typeArguments,
		});
	}

	init_claim(
		txb: TransactionBlock,
		{
			arguments: [store],
		}: {
			arguments: [store: TransactionObjectArgument | string];
		},
	) {
		const [bag, claimProof] = txb.moveCall({
			target: `${this.#package}::${this.#module}::init_claim`,
			arguments: [txb.object(store)],
		});

		return [bag, claimProof] as const;
	}

	reclaim(
		txb: TransactionBlock,
		{
			arguments: [store, receiver],
		}: {
			arguments: [
				store: TransactionObjectArgument | string,
				receiver: TransactionArgument | string,
			];
		},
	) {
		const [bag, claimProof] = txb.moveCall({
			target: `${this.#package}::${this.#module}::reclaim`,
			arguments: [
				txb.object(store),
				typeof receiver === 'string' ? txb.pure.address(receiver) : receiver,
			],
		});

		return [bag, claimProof] as const;
	}

	claim(
		txb: TransactionBlock,
		{
			arguments: [bag, claim, id],
			typeArguments,
		}: {
			arguments: [
				bag: TransactionObjectArgument | string,
				claim: Extract<TransactionArgument, { kind: 'NestedResult' }>,
				id: TransactionObjectArgument | string,
			];
			typeArguments: [string];
		},
	): Extract<TransactionArgument, { kind: 'Result' }> {
		return txb.moveCall({
			target: `${this.#package}::${this.#module}::claim`,
			arguments: [txb.object(bag), txb.object(claim), typeof id === 'string' ? txb.object(id) : id],
			typeArguments,
		});
	}

	finalize(
		txb: TransactionBlock,
		{
			arguments: [bag, claim],
		}: {
			arguments: [
				bag: TransactionObjectArgument | string,
				claim: Extract<TransactionArgument, { kind: 'NestedResult' }>,
			];
		},
	) {
		txb.moveCall({
			target: `${this.#package}::${this.#module}::finalize`,
			arguments: [txb.object(bag), txb.object(claim)],
		});
	}

	update_receiver(
		txb: TransactionBlock,
		{
			arguments: [bag, from, to],
		}: {
			arguments: [
				bag: TransactionObjectArgument | string,
				from: TransactionArgument | string,
				to: TransactionArgument | string,
			];
		},
	) {
		txb.moveCall({
			target: `${this.#package}::${this.#module}::update_receiver`,
			arguments: [
				txb.object(bag),
				typeof from === 'string' ? txb.pure.address(from) : from,
				typeof to === 'string' ? txb.pure.address(to) : to,
			],
		});
	}
}
