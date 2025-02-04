// This file is part of Acala.

// Copyright (C) 2020-2021 Acala Foundation.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! # Homa Lite Module
//! The Homa Lite module handles logic that allows the users to lock in KSM tokens on the Karura
//! Acala Chain, and mint LKSM tokens from the liquidity. The locked KSM are then used for Staking -
//! they will be used to nominate our partner Validators on the Kusama Chain.
//!
//! As the first draft, this module currently does not support Redeem function from LKSM to KSM.
//!
//! General workflow:
//! 1. User moves KSM cross-chain into the Karura chain
//! 2. User "Lock" their KSM on the Karura chain
//! 3. Karura send XCM back into Kusama chain, and Nominate these KSMs against our partner
//! Validators. 4. Karura mint LKSM on the Karura chain

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::unused_unit)]

pub mod benchmarking;
mod mock;
mod tests;
pub mod weights;

use frame_support::{pallet_prelude::*, transactional, PalletId};
use frame_system::{ensure_signed, pallet_prelude::*};
use module_support::Ratio;
use orml_traits::MultiCurrency;
use primitives::{Balance, CurrencyId, EraIndex};
use sp_runtime::{ArithmeticError, FixedPointNumber};
use sp_std::prelude::*;

pub use module::*;
pub use weights::WeightInfo;

/// Used to record the total issuance of the currencies during a batch.
/// This info is used to calculate exchange rate between Staking and Liquid currencies.
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq)]
pub struct TotalIssuanceInfo {
	pub staking_total: Balance,
	pub liquid_total: Balance,
}

#[frame_support::pallet]
pub mod module {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;

		/// Multi-currency support for asset management
		type Currency: MultiCurrency<Self::AccountId, CurrencyId = CurrencyId, Balance = Balance>;

		/// The Currency ID for the Staking asset
		#[pallet::constant]
		type StakingCurrencyId: Get<CurrencyId>;

		/// The Currency ID for the Liquid asset
		#[pallet::constant]
		type LiquidCurrencyId: Get<CurrencyId>;

		/// The ID for this pallet
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Origin used to Issue LKSM
		type IssuerOrigin: EnsureOrigin<Self::Origin>;

		/// Origin represented by the Root or Governance
		type GovernanceOrigin: EnsureOrigin<Self::Origin>;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The current Batch has not been processed, therefore the Liquid currency have not
		/// been issued yet.
		LiquidCurrencyNotIssuedForThisBatch,
		/// The relay chain's stash account have not been set.
		RelayChainStashAccountNotSet,
		/// The total issuance for the Staking currency must be more than zero.
		InvalidStakedCurrencyTotalIssuance,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	#[pallet::metadata(T::AccountId = "AccountId")]
	pub enum Event<T: Config> {
		/// The user has requested some Staking currency to be used to mint Liquid Currency.
		/// \[batch, user, amount\]
		MintRequested(EraIndex, T::AccountId, Balance),

		/// The current batch has been processed. Mint requests can now be completed. \[batch,
		/// staking_total_issuance, liquid_total_issuance\]
		BatchProcessed(EraIndex, Balance, Balance),

		/// The user has claimed some Liquid Currency. \[batch, user, amount\]
		LiquidCurrencyClaimed(EraIndex, T::AccountId, Balance),

		/// The relay chain's stash account ID has been updated.\[new_stash_account\]
		RelayChainStashAccountUpdated(T::AccountId),
	}

	/// Stores the amount of Staking currency the user has exchanged.
	/// PendingAmount: double_map: (batch: EraIndex, user: T::AccountId) -> amount: Balance
	#[pallet::storage]
	#[pallet::getter(fn pending_amount)]
	pub type PendingAmount<T: Config> =
		StorageDoubleMap<_, Twox64Concat, EraIndex, Blake2_128Concat, T::AccountId, Balance, ValueQuery>;

	/// The total issuance info for each batch. Used to calculate Staking to Liquid exchange rate.
	/// BatchTotalIssuanceInfo: map: batch: EraIndex -> batch_total: TotalIssuanceInfo
	#[pallet::storage]
	#[pallet::getter(fn batch_total_issuance_info)]
	pub type BatchTotalIssuanceInfo<T: Config> = StorageMap<_, Twox64Concat, EraIndex, TotalIssuanceInfo, OptionQuery>;

	/// The batch that is currency active
	/// CurrentBatch: value: batch: EraIndex
	#[pallet::storage]
	#[pallet::getter(fn current_batch)]
	pub type CurrentBatch<T: Config> = StorageValue<_, EraIndex, ValueQuery>;

	/// The account in which the staking currency goes into to be transferred to the Relay chain.
	/// RelayChainStashAccount: value: stash_account: AccountId
	#[pallet::storage]
	#[pallet::getter(fn relay_chain_stash_account)]
	pub type RelayChainStashAccount<T: Config> = StorageValue<_, T::AccountId, OptionQuery>;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Request to mint some Liquid currency, by locking up the given amount of Staking
		/// currency. The exchange does not happen immediately, but on when the batch is processed
		/// The user then needs to manually claim the Liquid currency once it is ready.
		///
		/// Parameters:
		/// - `amount`: The amount of Staking currency to be exchanged.
		#[pallet::weight(< T as Config >::WeightInfo::request_mint())]
		#[transactional]
		pub fn request_mint(origin: OriginFor<T>, amount: Balance) -> DispatchResult {
			let stash_account = Self::relay_chain_stash_account().ok_or(Error::<T>::RelayChainStashAccountNotSet)?;

			let who = ensure_signed(origin)?;
			let current_batch = Self::current_batch();
			let staking_currency_id = T::StakingCurrencyId::get();

			// TODO: Cross-chain transfer to the relay chain via XCM
			T::Currency::transfer(staking_currency_id, &who, &stash_account, amount)?;

			PendingAmount::<T>::mutate(current_batch, &who, |current| {
				*current = current.checked_add(amount).expect("Amount should not cause overflow.")
			});

			Self::deposit_event(Event::<T>::MintRequested(current_batch, who, amount));
			Ok(())
		}

		/// Process a batch.
		/// It is then that we can issue Liquid currencies.
		/// Requires `T::IssuerOrigin`
		///
		/// Parameters:
		/// - `staking_total`:
		#[pallet::weight(< T as Config >::WeightInfo::issue())]
		#[transactional]
		pub fn issue(origin: OriginFor<T>, staking_total: Balance) -> DispatchResult {
			T::IssuerOrigin::ensure_origin(origin)?;
			ensure!(staking_total != 0, Error::<T>::InvalidStakedCurrencyTotalIssuance);

			let current_batch = Self::current_batch();

			let liquid_total = T::Currency::total_issuance(T::LiquidCurrencyId::get());
			let total_for_batch = TotalIssuanceInfo {
				staking_total,
				liquid_total,
			};

			BatchTotalIssuanceInfo::<T>::insert(&current_batch, total_for_batch);
			CurrentBatch::<T>::put(current_batch.checked_add(1).expect("Batch Index should not overflow."));

			Self::deposit_event(Event::<T>::BatchProcessed(current_batch, staking_total, liquid_total));

			Ok(())
		}

		/// A function that allows the user to claim the Liquid currencies minted.
		/// The amount of liquid currency minted is proportional to the ratio of the total issuance
		/// of the staking and liquid currency.
		///
		/// Parameters:
		/// - `who`: The user the claimed Liquid currency is for.
		/// - `batch`: The batch index the user Staked their tokens.
		#[pallet::weight(< T as Config >::WeightInfo::claim())]
		#[transactional]
		pub fn claim(origin: OriginFor<T>, who: T::AccountId, batch: EraIndex) -> DispatchResult {
			ensure_signed(origin)?;
			let staked_amount = Self::pending_amount(&batch, &who);
			let total_info =
				Self::batch_total_issuance_info(batch).ok_or(Error::<T>::LiquidCurrencyNotIssuedForThisBatch)?;

			// liquid_to_mint = staked_amount * liquid_total / staked_total
			let exchange_ratio = Ratio::checked_from_rational(total_info.liquid_total, total_info.staking_total)
				.ok_or(ArithmeticError::Overflow)?;

			let liquid_to_mint = exchange_ratio
				.checked_mul_int(staked_amount)
				.ok_or(ArithmeticError::Overflow)?;

			// Mint the liquid currency into the user's account.
			T::Currency::deposit(T::LiquidCurrencyId::get(), &who, liquid_to_mint)?;
			// Remove the pending request from storage
			PendingAmount::<T>::remove(&batch, &who);

			Self::deposit_event(Event::<T>::LiquidCurrencyClaimed(batch, who, liquid_to_mint));

			Ok(())
		}

		/// Updates the relay chain Stash Account ID.
		/// Requires `T::GovernanceOrigin`
		///
		/// Parameters:
		/// - `new_account_id`: The new relay chain stash account.
		#[pallet::weight(< T as Config >::WeightInfo::set_stash_account_id())]
		#[transactional]
		pub fn set_stash_account_id(origin: OriginFor<T>, new_account_id: T::AccountId) -> DispatchResult {
			// This can only be called by Governance or ROOT.
			T::GovernanceOrigin::ensure_origin(origin)?;

			RelayChainStashAccount::<T>::put(new_account_id.clone());
			Self::deposit_event(Event::<T>::RelayChainStashAccountUpdated(new_account_id));
			Ok(())
		}
	}
}
