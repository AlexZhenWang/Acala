//! # CDP Engine Module
//!
//! ## Overview
//!
//! The core module of Honzon protocol. CDP engine is responsible for handle internal processes about CDPs,
//! including liquidation, settlement and risk management.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
	debug, decl_error, decl_event, decl_module, decl_storage, ensure,
	traits::{EnsureOrigin, Get},
	weights::DispatchClass,
	IterableStorageDoubleMap,
};
use frame_system::{
	self as system, ensure_none, ensure_root,
	offchain::{SendTransactionTypes, SubmitTransaction},
};
use orml_traits::Change;
use primitives::{Amount, Balance, CurrencyId};
use sp_runtime::{
	traits::{BlakeTwo256, Convert, Hash, Saturating, UniqueSaturatedInto, Zero},
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity, ValidTransaction,
	},
	DispatchResult, FixedPointNumber, RandomNumberGenerator, RuntimeDebug,
};
use sp_std::{marker, prelude::*};
use support::{
	CDPTreasury, CDPTreasuryExtended, DEXManager, ExchangeRate, OnEmergencyShutdown, Price, PriceProvider, Rate, Ratio,
	RiskManager,
};
use utilities::{LockItem, OffchainErr, OffchainLock};

mod debit_exchange_rate_convertor;
pub use debit_exchange_rate_convertor::DebitExchangeRateConvertor;

mod mock;
mod tests;

const DB_PREFIX: &[u8] = b"acala/cdp-engine-offchain-worker/";

pub trait Trait: SendTransactionTypes<Call<Self>> + system::Trait + loans::Trait {
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	/// The origin which may update risk management parameters. Root can always do this.
	type UpdateOrigin: EnsureOrigin<Self::Origin>;

	/// The list of valid collateral currency types
	type CollateralCurrencyIds: Get<Vec<CurrencyId>>;

	/// The default liquidation ratio for all collateral types of CDP
	type DefaultLiquidationRatio: Get<Ratio>;

	/// The default debit exchange rate for all collateral types
	type DefaultDebitExchangeRate: Get<ExchangeRate>;

	/// The default liquidation penalty rate when liquidate unsafe CDP
	type DefaultLiquidationPenalty: Get<Rate>;

	/// The minimum debit value to avoid debit dust
	type MinimumDebitValue: Get<Balance>;

	/// Stablecoin currency id
	type GetStableCurrencyId: Get<CurrencyId>;

	/// The max slippage allowed when liquidate an unsafe CDP by swap with DEX
	type MaxSlippageSwapWithDEX: Get<Ratio>;

	/// The CDP treasury to maintain bad debts and surplus generated by CDPs
	type CDPTreasury: CDPTreasuryExtended<Self::AccountId, Balance = Balance, CurrencyId = CurrencyId>;

	/// The price source of all types of currencies related to CDP
	type PriceSource: PriceProvider<CurrencyId>;

	/// The DEX participating in liquidation
	type DEX: DEXManager<Self::AccountId, CurrencyId, Balance>;

	/// A configuration for base priority of unsigned transactions.
	///
	/// This is exposed so that it can be tuned for particular runtime, when
	/// multiple modules send unsigned transactions.
	type UnsignedPriority: Get<TransactionPriority>;
}

/// Liquidation strategy available
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq)]
pub enum LiquidationStrategy {
	/// Liquidation CDP's collateral by create collateral auction
	Auction,
	/// Liquidation CDP's collateral by swap with DEX
	Exchange,
}

/// Risk management params
#[derive(Encode, Decode, Clone, RuntimeDebug, PartialEq, Eq, Default)]
pub struct RiskManagementParams {
	/// Maximum total debit value generated from it, when reach the hard cap,
	/// CDP's owner cannot issue more stablecoin under the collateral type.
	pub maximum_total_debit_value: Balance,

	/// Extra stability fee rate, `None` value means not set
	pub stability_fee: Option<Rate>,

	/// Liquidation ratio, when the collateral ratio of
	/// CDP under this collateral type is below the liquidation ratio, this CDP is unsafe and can be liquidated.
	/// `None` value means not set
	pub liquidation_ratio: Option<Ratio>,

	/// Liquidation penalty rate, when liquidation occurs,
	/// CDP will be deducted an additional penalty base on the product of penalty rate and debit value.
	/// `None` value means not set
	pub liquidation_penalty: Option<Rate>,

	/// Required collateral ratio, it it's set, cannot adjust the position of CDP so that
	/// the current collateral ratio is lower than the required collateral ratio.
	/// `None` value means not set
	pub required_collateral_ratio: Option<Ratio>,
}

// typedef to help polkadot.js disambiguate Change with different generic parameters
type ChangeOptionRate = Change<Option<Rate>>;
type ChangeOptionRatio = Change<Option<Ratio>>;
type ChangeBalance = Change<Balance>;

decl_event!(
	pub enum Event<T>
	where
		<T as system::Trait>::AccountId,
		CurrencyId = CurrencyId,
		Balance = Balance,
	{
		/// Liquidate the unsafe CDP (collateral_type, owner, collateral_amount, bad_debt_value, liquidation_strategy)
		LiquidateUnsafeCDP(CurrencyId, AccountId, Balance, Balance, LiquidationStrategy),
		/// Settle the CDP has debit (collateral_type, owner)
		SettleCDPInDebit(CurrencyId, AccountId),
		/// The stability fee for specific collateral type updated (collateral_type, new_stability_fee)
		StabilityFeeUpdated(CurrencyId, Option<Rate>),
		/// The liquidation fee for specific collateral type updated (collateral_type, new_liquidation_ratio)
		LiquidationRatioUpdated(CurrencyId, Option<Ratio>),
		/// The liquidation penalty rate for specific collateral type updated (collateral_type, new_liquidation_panelty)
		LiquidationPenaltyUpdated(CurrencyId, Option<Rate>),
		/// The required collateral penalty rate for specific collateral type updated (collateral_type, new_required_collateral_ratio)
		RequiredCollateralRatioUpdated(CurrencyId, Option<Ratio>),
		/// The hard cap of total debit value for specific collateral type updated (collateral_type, new_total_debit_value)
		MaximumTotalDebitValueUpdated(CurrencyId, Balance),
		/// The global stability fee for all types of collateral updated (new_global_stability_fee)
		GlobalStabilityFeeUpdated(Rate),
	}
);

decl_error! {
	/// Error for cdp engine module.
	pub enum Error for Module<T: Trait> {
		/// The total debit value of specific collateral type already exceed the hard cap
		ExceedDebitValueHardCap,
		/// The collateral ratio below the required collateral ratio
		BelowRequiredCollateralRatio,
		/// The collateral ratio below the liquidation ratio
		BelowLiquidationRatio,
		/// The CDP must be unsafe to be liquidated
		MustBeUnsafe,
		/// Invalid collateral type
		InvalidCollateralType,
		/// Remain debit value in CDP below the dust amount
		RemainDebitValueTooSmall,
		/// Feed price is invalid
		InvalidFeedPrice,
		/// No debit value in CDP so that it cannot be settled
		NoDebitValue,
		/// System has already been shutdown
		AlreadyShutdown,
		/// Must after system shutdown
		MustAfterShutdown,
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as CDPEngine {
		/// System shutdown flag
		pub IsShutdown get(fn is_shutdown): bool;

		/// Mapping from collateral type to its exchange rate of debit units and debit value
		pub DebitExchangeRate get(fn debit_exchange_rate): map hasher(twox_64_concat) CurrencyId => Option<ExchangeRate>;

		/// Global stability fee rate for all types of collateral
		pub GlobalStabilityFee get(fn global_stability_fee) config(): Rate;

		/// Mapping from collateral type to its risk management params
		pub CollateralParams get(fn collateral_params): map hasher(twox_64_concat) CurrencyId => RiskManagementParams;
	}

	add_extra_genesis {
		#[allow(clippy::type_complexity)] // it's reasonable to use this one-off complex params config type
		config(collaterals_params): Vec<(CurrencyId, Option<Rate>, Option<Ratio>, Option<Rate>, Option<Ratio>, Balance)>;
		build(|config: &GenesisConfig| {
			config.collaterals_params.iter().for_each(|(
				currency_id,
				stability_fee,
				liquidation_ratio,
				liquidation_penalty,
				required_collateral_ratio,
				maximum_total_debit_value,
			)| {
				CollateralParams::insert(currency_id, RiskManagementParams {
					maximum_total_debit_value: *maximum_total_debit_value,
					stability_fee: *stability_fee,
					liquidation_ratio: *liquidation_ratio,
					liquidation_penalty: *liquidation_penalty,
					required_collateral_ratio: *required_collateral_ratio,
				});
			});
		});
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;
		fn deposit_event() = default;

		/// The list of valid collateral currency types
		const CollateralCurrencyIds: Vec<CurrencyId> = T::CollateralCurrencyIds::get();

		/// The minimum debit value allowed exists in CDP which has debit amount to avoid dust
		const MinimumDebitValue: Balance = T::MinimumDebitValue::get();

		/// The stable currency id
		const GetStableCurrencyId: CurrencyId = T::GetStableCurrencyId::get();

		/// The max slippage allowed when liquidate an unsafe CDP by swap with DEX
		const MaxSlippageSwapWithDEX: Ratio = T::MaxSlippageSwapWithDEX::get();

		/// The default liquidation ratio for all collateral types of CDP,
		/// if the liquidation ratio for specific collateral is `None`, it works.
		const DefaultLiquidationRatio: Ratio = T::DefaultLiquidationRatio::get();

		/// The default debit exchange rate for all collateral types,
		/// if the debit exchange rate for specific collateral is `None`, it works.
		const DefaultDebitExchangeRate: ExchangeRate = T::DefaultDebitExchangeRate::get();

		/// The default liquidation penalty rate when liquidate unsafe CDP,
		/// if the liquidation penalty rate for specific collateral is `None`, it works.
		const DefaultLiquidationPenalty: Rate = T::DefaultLiquidationPenalty::get();

		/// Liquidate unsafe CDP
		///
		/// The dispatch origin of this call must be _None_.
		///
		/// - `currency_id`: CDP's collateral type.
		/// - `who`: CDP's owner.
		///
		/// # <weight>
		/// - Preconditions:
		/// 	- T::CDPTreasury is module_cdp_treasury
		/// 	- T::DEX is module_dex
		/// - Complexity: `O(1)`
		/// - Db reads:
		///		- liquidate by auction: `IsShutdown`, (4 + 2 + 3 + 2 + 1 + 3 + 2) items of modules related to module_cdp_engine
		///		- liquidate by dex: `IsShutdown`, (4 + 5 + 3 + 2 + 2 + 0 + 2) items of modules related to module_cdp_engine
		/// - Db writes:
		///		- liquidate by auction: (4 + 2 + 0 + 2 + 0 + 5) items of modules related to module_cdp_engine
		///		- liquidate by dex: (4 + 5 + 0 + 2 + 1 + 0) items of modules related to module_cdp_engine
		/// -------------------
		/// Base Weight:
		///		- liquidate by auction: 119.4 µs
		///		- liquidate by dex: 125.1 µs
		/// # </weight>
		#[weight = (125_000_000 + T::DbWeight::get().reads_writes(18, 13), DispatchClass::Operational)]
		pub fn liquidate(
			origin,
			currency_id: CurrencyId,
			who: T::AccountId,
		) {
			ensure_none(origin)?;
			ensure!(!Self::is_shutdown(), Error::<T>::AlreadyShutdown);
			Self::liquidate_unsafe_cdp(who, currency_id)?;
		}

		/// Settle CDP has debit after system shutdown
		///
		/// The dispatch origin of this call must be _None_.
		///
		/// - `currency_id`: CDP's collateral type.
		/// - `who`: CDP's owner.
		///
		/// # <weight>
		/// - Preconditions:
		/// 	- T::CDPTreasury is module_cdp_treasury
		/// 	- T::DEX is module_dex
		/// - Complexity: `O(1)`
		/// - Db reads: `IsShutdown`, 9 items of modules related to module_cdp_engine
		/// - Db writes: 8 items of modules related to module_cdp_engine
		/// -------------------
		/// Base Weight: 76.54 µs
		/// # </weight>
		#[weight = (77_000_000 + T::DbWeight::get().reads_writes(10, 8), DispatchClass::Operational)]
		pub fn settle(
			origin,
			currency_id: CurrencyId,
			who: T::AccountId,
		) {
			ensure_none(origin)?;
			ensure!(Self::is_shutdown(), Error::<T>::MustAfterShutdown);
			Self::settle_cdp_has_debit(who, currency_id)?;
		}

		/// Update global parameters related to risk management of CDP
		///
		/// The dispatch origin of this call must be `UpdateOrigin` or _Root_.
		///
		/// - `global_stability_fee`: global stability fee rate.
		///
		/// # <weight>
		/// - Complexity: `O(1)`
		/// - Db reads:
		/// - Db writes: `GlobalStabilityFee`
		/// -------------------
		/// Base Weight: 21.04 µs
		/// # </weight>
		#[weight = 21_000_000 + T::DbWeight::get().reads_writes(0, 1)]
		pub fn set_global_params(
			origin,
			global_stability_fee: Rate,
		) {
			T::UpdateOrigin::try_origin(origin)
				.map(|_| ())
				.or_else(ensure_root)?;
			GlobalStabilityFee::put(global_stability_fee);
			Self::deposit_event(RawEvent::GlobalStabilityFeeUpdated(global_stability_fee));
		}

		/// Update parameters related to risk management of CDP under specific collateral type
		///
		/// The dispatch origin of this call must be `UpdateOrigin` or _Root_.
		///
		/// - `currency_id`: collateral type.
		/// - `stability_fee`: extra stability fee rate, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `liquidation_ratio`: liquidation ratio, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `liquidation_penalty`: liquidation penalty, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `required_collateral_ratio`: required collateral ratio, `None` means do not update, `Some(None)` means update it to `None`.
		/// - `maximum_total_debit_value`: maximum total debit value.
		///
		/// # <weight>
		/// - Complexity: `O(1)`
		/// - Db reads:	`CollateralParams`
		/// - Db writes: `CollateralParams`
		/// -------------------
		/// Base Weight: 32.81 µs
		/// # </weight>
		#[weight = 33_000_000 + T::DbWeight::get().reads_writes(1, 1)]
		pub fn set_collateral_params(
			origin,
			currency_id: CurrencyId,
			stability_fee: ChangeOptionRate,
			liquidation_ratio: ChangeOptionRatio,
			liquidation_penalty: ChangeOptionRate,
			required_collateral_ratio: ChangeOptionRatio,
			maximum_total_debit_value: ChangeBalance,
		) {
			T::UpdateOrigin::try_origin(origin)
				.map(|_| ())
				.or_else(ensure_root)?;
			ensure!(
				T::CollateralCurrencyIds::get().contains(&currency_id),
				Error::<T>::InvalidCollateralType,
			);

			let mut collateral_params = Self::collateral_params(currency_id);
			if let Change::NewValue(update) = stability_fee {
				collateral_params.stability_fee = update;
				Self::deposit_event(RawEvent::StabilityFeeUpdated(currency_id, update));
			}
			if let Change::NewValue(update) = liquidation_ratio {
				collateral_params.liquidation_ratio = update;
				Self::deposit_event(RawEvent::LiquidationRatioUpdated(currency_id, update));
			}
			if let Change::NewValue(update) = liquidation_penalty {
				collateral_params.liquidation_penalty = update;
				Self::deposit_event(RawEvent::LiquidationPenaltyUpdated(currency_id, update));
			}
			if let Change::NewValue(update) = required_collateral_ratio {
				collateral_params.required_collateral_ratio = update;
				Self::deposit_event(RawEvent::RequiredCollateralRatioUpdated(currency_id, update));
			}
			if let Change::NewValue(val) = maximum_total_debit_value {
				collateral_params.maximum_total_debit_value = val;
				Self::deposit_event(RawEvent::MaximumTotalDebitValueUpdated(currency_id, val));
			}
			CollateralParams::insert(currency_id, collateral_params);
		}

		/// Issue interest in stable coin for all types of collateral has debit when block end,
		/// and update their debit exchange rate
		fn on_finalize(_now: T::BlockNumber) {
			// collect stability fee for all types of collateral
			if !Self::is_shutdown() {
				for currency_id in T::CollateralCurrencyIds::get() {
					let debit_exchange_rate = Self::get_debit_exchange_rate(currency_id);
					let stability_fee_rate = Self::get_stability_fee(currency_id);
					let total_debits = <loans::Module<T>>::total_debits(currency_id);
					if !stability_fee_rate.is_zero() && !total_debits.is_zero() {
						let debit_exchange_rate_increment = debit_exchange_rate.saturating_mul(stability_fee_rate);
						let total_debit_value = Self::get_debit_value(currency_id, total_debits);
						let issued_stable_coin_balance = debit_exchange_rate_increment.saturating_mul_int(total_debit_value);

						// issue stablecoin to surplus pool
						if <T as Trait>::CDPTreasury::on_system_surplus(issued_stable_coin_balance).is_ok() {
							// update exchange rate when issue success
							let new_debit_exchange_rate = debit_exchange_rate.saturating_add(debit_exchange_rate_increment);
							DebitExchangeRate::insert(currency_id, new_debit_exchange_rate);
						}
					}
				}
			}
		}

		/// Runs after every block. Start offchain worker to check CDP and
		/// submit unsigned tx to trigger liquidation or settlement.
		fn offchain_worker(now: T::BlockNumber) {
			if let Err(e) = Self::_offchain_worker(now) {
				debug::info!(
					target: "cdp-engine offchain worker",
					"cannot run offchain worker at {:?}: {:?}",
					now,
					e,
				);
			}
		}
	}
}

impl<T: Trait> Module<T> {
	fn submit_unsigned_liquidation_tx(currency_id: CurrencyId, who: T::AccountId) -> Result<(), OffchainErr> {
		let call = Call::<T>::liquidate(currency_id, who);
		SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into())
			.map_err(|_| OffchainErr::SubmitTransaction)?;
		Ok(())
	}

	fn submit_unsigned_settle_tx(currency_id: CurrencyId, who: T::AccountId) -> Result<(), OffchainErr> {
		let call = Call::<T>::settle(currency_id, who);
		SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into())
			.map_err(|_| OffchainErr::SubmitTransaction)?;
		Ok(())
	}

	fn _offchain_worker(block_number: T::BlockNumber) -> Result<(), OffchainErr> {
		let collateral_currency_ids = T::CollateralCurrencyIds::get();
		if collateral_currency_ids.len().is_zero() {
			return Ok(());
		}

		// check if we are a potential validator
		if !sp_io::offchain::is_validator() {
			return Err(OffchainErr::NotValidator);
		}

		let collateral_currency_ids = T::CollateralCurrencyIds::get();
		let offchain_lock = OffchainLock::new(DB_PREFIX.to_vec());

		// Acquire offchain worker lock.
		// If succeeded, update the lock, otherwise return error
		let LockItem {
			extra_data: position, ..
		} = offchain_lock.acquire_offchain_lock(|val: Option<u32>| {
			if let Some(previous_position) = val {
				if previous_position < collateral_currency_ids.len().saturating_sub(1) as u32 {
					previous_position + 1
				} else {
					0
				}
			} else {
				let random_seed = sp_io::offchain::random_seed();
				let mut rng = RandomNumberGenerator::<BlakeTwo256>::new(BlakeTwo256::hash(&random_seed[..]));

				rng.pick_u32(collateral_currency_ids.len().saturating_sub(1) as u32)
			}
		})?;

		let currency_id = collateral_currency_ids[(position as usize)];

		if !Self::is_shutdown() {
			for (account_id, _) in <loans::Debits<T>>::iter_prefix(currency_id) {
				if Self::is_cdp_unsafe(currency_id, &account_id) {
					if let Err(e) = Self::submit_unsigned_liquidation_tx(currency_id, account_id.clone()) {
						debug::warn!(
							target: "cdp-engine offchain worker",
							"submit unsigned liquidation tx for \nCDP - AccountId {:?} CurrencyId {:?} \nfailed : {:?}",
							account_id, currency_id, e,
						);
					} else {
						debug::debug!(
							target: "cdp-engine offchain worker",
							"successfully submit unsigned liquidation tx for \nCDP - AccountId {:?} CurrencyId {:?}",
							account_id, currency_id,
						);
					}
				}

				// check the expire timestamp of lock that is needed to extend
				offchain_lock.extend_offchain_lock_if_needed::<u32>();
			}
		} else {
			for (account_id, debit) in <loans::Debits<T>>::iter_prefix(currency_id) {
				if !debit.is_zero() {
					if let Err(e) = Self::submit_unsigned_settle_tx(currency_id, account_id.clone()) {
						debug::warn!(
							target: "cdp-engine offchain worker",
							"submit unsigned settlement tx for \nCDP - AccountId {:?} CurrencyId {:?} \nfailed : {:?}",
							account_id, currency_id, e,
						);
					} else {
						debug::debug!(
							target: "cdp-engine offchain worker",
							"successfully submit unsigned settlement tx for \nCDP - AccountId {:?} CurrencyId {:?}",
							account_id, currency_id,
						);
					}
				}

				// check the expire timestamp of lock that is needed to extend
				offchain_lock.extend_offchain_lock_if_needed::<u32>();
			}
		}

		// finally, reset the expire timestamp to now in order to release lock in advance.
		offchain_lock.release_offchain_lock(|current_position: u32| current_position == position);
		debug::debug!(
			target: "cdp-engine offchain worker",
			"offchain worker start at block: {:?} already done!",
			block_number,
		);

		Ok(())
	}

	pub fn is_cdp_unsafe(currency_id: CurrencyId, who: &T::AccountId) -> bool {
		let debit_balance = <loans::Module<T>>::debits(currency_id, who);
		let collateral_balance = <loans::Module<T>>::collaterals(who, currency_id);
		let stable_currency_id = T::GetStableCurrencyId::get();

		if debit_balance.is_zero() {
			false
		} else if let Some(feed_price) = T::PriceSource::get_relative_price(currency_id, stable_currency_id) {
			let collateral_ratio =
				Self::calculate_collateral_ratio(currency_id, collateral_balance, debit_balance, feed_price);
			collateral_ratio < Self::get_liquidation_ratio(currency_id)
		} else {
			// if feed_price is invalid, can not judge the cdp is safe or unsafe!
			false
		}
	}

	pub fn maximum_total_debit_value(currency_id: CurrencyId) -> Balance {
		Self::collateral_params(currency_id).maximum_total_debit_value
	}

	pub fn required_collateral_ratio(currency_id: CurrencyId) -> Option<Ratio> {
		Self::collateral_params(currency_id).required_collateral_ratio
	}

	pub fn get_stability_fee(currency_id: CurrencyId) -> Rate {
		Self::collateral_params(currency_id)
			.stability_fee
			.unwrap_or_default()
			.saturating_add(Self::global_stability_fee())
	}

	pub fn get_liquidation_ratio(currency_id: CurrencyId) -> Ratio {
		Self::collateral_params(currency_id)
			.liquidation_ratio
			.unwrap_or_else(T::DefaultLiquidationRatio::get)
	}

	pub fn get_liquidation_penalty(currency_id: CurrencyId) -> Rate {
		Self::collateral_params(currency_id)
			.liquidation_penalty
			.unwrap_or_else(T::DefaultLiquidationPenalty::get)
	}

	pub fn get_debit_exchange_rate(currency_id: CurrencyId) -> ExchangeRate {
		Self::debit_exchange_rate(currency_id).unwrap_or_else(T::DefaultDebitExchangeRate::get)
	}

	pub fn get_debit_value(currency_id: CurrencyId, debit_balance: T::DebitBalance) -> Balance {
		DebitExchangeRateConvertor::<T>::convert((currency_id, debit_balance))
	}

	pub fn calculate_collateral_ratio(
		currency_id: CurrencyId,
		collateral_balance: Balance,
		debit_balance: T::DebitBalance,
		price: Price,
	) -> Ratio {
		let locked_collateral_value = price.saturating_mul_int(collateral_balance);
		let debit_value = Self::get_debit_value(currency_id, debit_balance);

		Ratio::checked_from_rational(locked_collateral_value, debit_value).unwrap_or_default()
	}

	pub fn adjust_position(
		who: &T::AccountId,
		currency_id: CurrencyId,
		collateral_adjustment: Amount,
		debit_adjustment: T::DebitAmount,
	) -> DispatchResult {
		ensure!(
			T::CollateralCurrencyIds::get().contains(&currency_id),
			Error::<T>::InvalidCollateralType,
		);
		<loans::Module<T>>::adjust_position(who, currency_id, collateral_adjustment, debit_adjustment)?;
		Ok(())
	}

	// settle cdp has debit when emergency shutdown
	pub fn settle_cdp_has_debit(who: T::AccountId, currency_id: CurrencyId) -> DispatchResult {
		let debit_balance = <loans::Module<T>>::debits(currency_id, &who);
		ensure!(!debit_balance.is_zero(), Error::<T>::NoDebitValue);

		// confiscate collateral in cdp to cdp treasury
		// and decrease cdp's debit to zero
		let collateral_balance = <loans::Module<T>>::collaterals(&who, currency_id);
		let settle_price: Price = T::PriceSource::get_relative_price(T::GetStableCurrencyId::get(), currency_id)
			.ok_or(Error::<T>::InvalidFeedPrice)?;
		let bad_debt_value = Self::get_debit_value(currency_id, debit_balance);
		let confiscate_collateral_amount =
			sp_std::cmp::min(settle_price.saturating_mul_int(bad_debt_value), collateral_balance);

		// confiscate collateral and all debit
		<loans::Module<T>>::confiscate_collateral_and_debit(
			&who,
			currency_id,
			confiscate_collateral_amount,
			debit_balance,
		)?;

		Self::deposit_event(RawEvent::SettleCDPInDebit(currency_id, who));
		Ok(())
	}

	// liquidate unsafe cdp
	pub fn liquidate_unsafe_cdp(who: T::AccountId, currency_id: CurrencyId) -> DispatchResult {
		let debit_balance = <loans::Module<T>>::debits(currency_id, &who);
		let collateral_balance = <loans::Module<T>>::collaterals(&who, currency_id);
		let stable_currency_id = T::GetStableCurrencyId::get();

		// ensure the cdp is unsafe
		ensure!(Self::is_cdp_unsafe(currency_id, &who), Error::<T>::MustBeUnsafe);

		// confiscate all collateral and debit of unsafe cdp to cdp treasury
		<loans::Module<T>>::confiscate_collateral_and_debit(&who, currency_id, collateral_balance, debit_balance)?;

		let bad_debt_value = Self::get_debit_value(currency_id, debit_balance);
		let target_stable_amount = bad_debt_value
			.saturating_add(Self::get_liquidation_penalty(currency_id).saturating_mul_int(bad_debt_value));
		let supply_collateral_amount = T::DEX::get_supply_amount(currency_id, stable_currency_id, target_stable_amount);
		let exchange_slippage =
			T::DEX::get_exchange_slippage(currency_id, stable_currency_id, supply_collateral_amount);
		let slippage_limit = T::MaxSlippageSwapWithDEX::get();

		// if collateral_balance can swap enough native token in DEX and exchange slippage is blow the limit,
		// directly exchange with DEX, otherwise create collateral auctions.
		let liquidation_strategy: LiquidationStrategy = if !supply_collateral_amount.is_zero() 	// supply_collateral_amount must not be zero
			&& collateral_balance >= supply_collateral_amount									// ensure have sufficient collateral
			&& slippage_limit > Ratio::zero()											// slippage_limit must be set as more than zero
			&& exchange_slippage.map_or(false, |s| s <= slippage_limit)
		{
			LiquidationStrategy::Exchange
		} else {
			LiquidationStrategy::Auction
		};

		match liquidation_strategy {
			LiquidationStrategy::Exchange => {
				if <T as Trait>::CDPTreasury::swap_collateral_to_stable(
					currency_id,
					supply_collateral_amount,
					target_stable_amount,
				)
				.is_ok()
				{
					// refund remain collateral to CDP owner
					let refund_collateral_amount = collateral_balance - supply_collateral_amount;
					if !refund_collateral_amount.is_zero() {
						<T as Trait>::CDPTreasury::transfer_collateral_to(currency_id, &who, refund_collateral_amount)
							.expect("never failed");
					}
				}
			}
			LiquidationStrategy::Auction => {
				// create collateral auctions by cdp treasury
				<T as Trait>::CDPTreasury::create_collateral_auctions(
					currency_id,
					collateral_balance,
					target_stable_amount,
					who.clone(),
				);
			}
		}

		Self::deposit_event(RawEvent::LiquidateUnsafeCDP(
			currency_id,
			who,
			collateral_balance,
			bad_debt_value,
			liquidation_strategy,
		));
		Ok(())
	}
}

impl<T: Trait> RiskManager<T::AccountId, CurrencyId, Balance, T::DebitBalance> for Module<T> {
	fn get_bad_debt_value(currency_id: CurrencyId, debit_balance: T::DebitBalance) -> Balance {
		Self::get_debit_value(currency_id, debit_balance)
	}

	fn check_position_valid(
		currency_id: CurrencyId,
		collateral_balance: Balance,
		debit_balance: T::DebitBalance,
	) -> DispatchResult {
		let debit_value = Self::get_debit_value(currency_id, debit_balance);

		if !debit_value.is_zero() {
			let feed_price = <T as Trait>::PriceSource::get_relative_price(currency_id, T::GetStableCurrencyId::get())
				.ok_or(Error::<T>::InvalidFeedPrice)?;
			let collateral_ratio =
				Self::calculate_collateral_ratio(currency_id, collateral_balance, debit_balance, feed_price);

			// check the required collateral ratio
			if let Some(required_collateral_ratio) = Self::required_collateral_ratio(currency_id) {
				ensure!(
					collateral_ratio >= required_collateral_ratio,
					Error::<T>::BelowRequiredCollateralRatio
				);
			}

			// check the liquidation ratio
			ensure!(
				collateral_ratio >= Self::get_liquidation_ratio(currency_id),
				Error::<T>::BelowLiquidationRatio
			);

			// check the minimum_debit_value
			ensure!(
				debit_value >= T::MinimumDebitValue::get(),
				Error::<T>::RemainDebitValueTooSmall,
			);
		}

		Ok(())
	}

	fn check_debit_cap(currency_id: CurrencyId, total_debit_balance: T::DebitBalance) -> DispatchResult {
		let hard_cap = Self::maximum_total_debit_value(currency_id);
		let total_debit_value = Self::get_debit_value(currency_id, total_debit_balance);

		ensure!(total_debit_value <= hard_cap, Error::<T>::ExceedDebitValueHardCap,);

		Ok(())
	}
}

impl<T: Trait> OnEmergencyShutdown for Module<T> {
	fn on_emergency_shutdown() {
		<IsShutdown>::put(true);
	}
}

#[allow(deprecated)]
impl<T: Trait> frame_support::unsigned::ValidateUnsigned for Module<T> {
	type Call = Call<T>;

	fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
		match call {
			Call::liquidate(currency_id, who) => {
				if !Self::is_cdp_unsafe(*currency_id, &who) || Self::is_shutdown() {
					return InvalidTransaction::Stale.into();
				}

				ValidTransaction::with_tag_prefix("CDPEngineOffchainWorker")
					.priority(T::UnsignedPriority::get())
					.and_provides((<system::Module<T>>::block_number(), currency_id, who))
					.longevity(64_u64)
					.propagate(true)
					.build()
			}
			Call::settle(currency_id, who) => {
				let debit_balance = <loans::Module<T>>::debits(currency_id, who);
				if debit_balance.is_zero() || !Self::is_shutdown() {
					return InvalidTransaction::Stale.into();
				}

				ValidTransaction::with_tag_prefix("CDPEngineOffchainWorker")
					.priority(T::UnsignedPriority::get())
					.and_provides((currency_id, who))
					.longevity(64_u64)
					.propagate(true)
					.build()
			}
			_ => InvalidTransaction::Call.into(),
		}
	}
}
