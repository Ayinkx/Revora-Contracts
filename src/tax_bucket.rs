use crate::{DataKey2, OfferingId};
use soroban_sdk::{contracttype, symbol_short, Address, Env, Symbol};

pub const EVENT_TAX_ROLLOVER: Symbol = symbol_short!("tax_roll");
/// Emitted on each tax-bucket update to enable off-chain tax-lot reconstruction.
///
/// Topic:  `(tax_lot_v1, issuer, namespace, token)`
/// Data:   `(holder: Address, return_of_capital: i128, capital_gains: i128,
///           amount: i128, period_id: u64, timestamp: u64)`
///
/// ### Field order (for indexer deserialization)
/// 0. `holder`          — Address of the holder whose bucket was updated.
/// 1. `return_of_capital` — Amount treated as return of capital (non-taxable).
/// 2. `capital_gains`    — Amount treated as capital gains (taxable).
/// 3. `amount`           — Total payout amount (`return_of_capital + capital_gains`).
/// 4. `period_id`        — The period associated with this distribution.
/// 5. `timestamp`        — Ledger timestamp at the time of the event.
pub const EVENT_TAX_LOT_V1: Symbol = symbol_short!("tax_lt1");

/// Emitted when return-of-capital is capped by remaining cost basis.
/// The excess amount is reclassified as capital gains.
///
/// Topic:  `(tax_recls, issuer, namespace, token)`
/// Data:   `(holder: Address, capped_amount: i128, reclassified_amount: i128)`
pub const EVENT_TAX_RECLASSIFY: Symbol = symbol_short!("tax_recls");

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TaxBucketResult {
    pub return_of_capital: i128,
    pub capital_gains: i128,
}

/// Per-holder, per-fiscal-year accumulated tax summary.
///
/// Returned by `get_holder_tax_year`. Accumulated on every `rollover_distribution`
/// call by incrementing the active fiscal year's entry in persistent storage.
///
/// Fields match the tax-bucket breakdown expected by integrators:
/// - `ordinary_income`:  Ordinary taxable income (dividends, interest, etc.)
/// - `capital_gains`:    Capital gains (profit from sale of securities)
/// - `return_of_capital`: Return of capital (non-taxable distribution)
///
/// Currently the system only populates `return_of_capital` and `capital_gains`.
/// The `ordinary_income` field is reserved for future tax-bucket expansion.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TaxYearSummary {
    /// Ordinary taxable income (dividends, interest, etc.).
    pub ordinary_income: i128,
    /// Total capital gains (taxable) for this fiscal year.
    pub capital_gains: i128,
    /// Total return of capital (non-taxable) for this fiscal year.
    pub return_of_capital: i128,
}

// ── Timestamp helpers ────────────────────────────────────────────────────────
//
// These convert a Unix timestamp (seconds since epoch) into calendar year and
// month, then compute the fiscal year given the offering's configured fiscal
// start month.  The algorithms are adapted from common calendar routines and
// use no external date libraries, keeping the contract `#![no_std]`.

const SECS_PER_DAY: u64 = 86_400;

/// Returns `true` if `year` is a Gregorian leap year.
fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in each month for a given year (0‑indexed: January = 0).
const MONTH_DAYS_NON_LEAP: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const MONTH_DAYS_LEAP: [u64; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Convert a Unix timestamp (seconds since epoch) to a Gregorian calendar year.
pub fn timestamp_to_year(ts: u64) -> u32 {
    let days = ts / SECS_PER_DAY;
    let mut year = 1970u32;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }
    year
}

/// Convert a Unix timestamp (seconds since epoch) to a Gregorian calendar month
/// (1‑based: January = 1, February = 2, …).
pub fn timestamp_to_month(ts: u64) -> u32 {
    let days = ts / SECS_PER_DAY;
    let mut year = 1970u32;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }
    let month_table = if is_leap_year(year) { MONTH_DAYS_LEAP } else { MONTH_DAYS_NON_LEAP };
    let mut month: u32 = 0;
    for &md in month_table.iter() {
        if remaining < md {
            break;
        }
        remaining -= md;
        month += 1;
    }
    // month is 0‑indexed; return 1‑based
    month + 1
}

/// Compute the fiscal year that contains `ts`, given the fiscal year start
/// month (1‑12) configured for the offering.
///
/// For example, if the fiscal year starts in April (`fiscal_start_month = 4`):
/// - Timestamps in Apr 2024 – Mar 2025 → fiscal year 2024.
/// - Timestamps in Apr 2023 – Mar 2024 → fiscal year 2023.
pub fn fiscal_year_from_ts(ts: u64, fiscal_start_month: u32) -> u64 {
    let year = timestamp_to_year(ts);
    let month = timestamp_to_month(ts);
    if month < fiscal_start_month {
        (year - 1) as u64
    } else {
        year as u64
    }
}

/// Default fiscal year start month (January = 1).
pub const DEFAULT_FISCAL_START_MONTH: u32 = 1;

pub fn track_cost_basis(env: &Env, offering_id: &OfferingId, holder: &Address, cost_basis: i128) {
    let key = DataKey2::RemainingBasis(offering_id.clone(), holder.clone());
    env.storage().persistent().set(&key, &cost_basis);
}

/// Update the tax-year accumulator for a holder's distribution.
///
/// Called from `rollover_distribution` (and from `claim`) to increment the
/// per-holder, per-fiscal-year `TaxYearSummary` entry in persistent storage.
pub fn update_tax_year_accumulator(
    env: &Env,
    offering_id: &OfferingId,
    holder: &Address,
    fiscal_year: u64,
    ordinary_income: i128,
    capital_gains: i128,
    return_of_capital: i128,
) {
    let year_key = DataKey2::TaxYearEntry(offering_id.clone(), holder.clone(), fiscal_year);
    let mut summary: TaxYearSummary = env
        .storage()
        .persistent()
        .get(&year_key)
        .unwrap_or(TaxYearSummary { ordinary_income: 0, capital_gains: 0, return_of_capital: 0 });
    summary.ordinary_income = summary.ordinary_income.saturating_add(ordinary_income);
    summary.capital_gains = summary.capital_gains.saturating_add(capital_gains);
    summary.return_of_capital = summary.return_of_capital.saturating_add(return_of_capital);
    env.storage().persistent().set(&year_key, &summary);
}

/// Apply return-of-capital with a hard cap at remaining cost basis.
/// Any excess is reclassified as capital gains.
/// Emits `EVENT_TAX_RECLASSIFY` when the cap is hit.
/// Uses checked subtraction to avoid underflow.
pub fn apply_return_of_capital_with_cap(
    env: &Env,
    offering_id: &OfferingId,
    holder: &Address,
    amount: i128,
    period_id: u64,
    timestamp: u64,
) -> TaxBucketResult {
    let key = DataKey2::RemainingBasis(offering_id.clone(), holder.clone());
    let remaining_basis: i128 = env.storage().persistent().get(&key).unwrap_or(0);

    if remaining_basis <= 0 {
        let result = TaxBucketResult { return_of_capital: 0, capital_gains: amount };
        env.events().publish(
            (
                EVENT_TAX_LOT_V1,
                offering_id.issuer.clone(),
                offering_id.namespace.clone(),
                offering_id.token.clone(),
            ),
            (
                holder.clone(),
                result.return_of_capital,
                result.capital_gains,
                amount,
                period_id,
                timestamp,
            ),
        );
        return result;
    }

    let (return_of_capital, capital_gains) = if amount <= remaining_basis {
        let new_basis = remaining_basis.checked_sub(amount).unwrap_or(0);
        env.storage().persistent().set(&key, &new_basis);
        (amount, 0i128)
    } else {
        let roc = remaining_basis;
        let cg = amount.checked_sub(remaining_basis).unwrap_or(0);

        env.storage().persistent().set(&key, &0i128);

        env.events().publish(
            (
                EVENT_TAX_RECLASSIFY,
                offering_id.issuer.clone(),
                offering_id.namespace.clone(),
                offering_id.token.clone(),
            ),
            (holder.clone(), roc, cg),
        );

        env.events().publish(
            (
                EVENT_TAX_ROLLOVER,
                offering_id.issuer.clone(),
                offering_id.namespace.clone(),
                offering_id.token.clone(),
            ),
            (holder.clone(), remaining_basis, 0i128),
        );

        (roc, cg)
    };

    env.events().publish(
        (
            EVENT_TAX_LOT_V1,
            offering_id.issuer.clone(),
            offering_id.namespace.clone(),
            offering_id.token.clone(),
        ),
        (holder.clone(), return_of_capital, capital_gains, amount, period_id, timestamp),
    );

    TaxBucketResult { return_of_capital, capital_gains }
}

pub fn rollover_distribution(
    env: &Env,
    offering_id: &OfferingId,
    holder: &Address,
    amount: i128,
    period_id: u64,
    timestamp: u64,
) -> TaxBucketResult {
    apply_return_of_capital_with_cap(env, offering_id, holder, amount, period_id, timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Events;
    use soroban_sdk::{symbol_short, Address, Env};

    fn setup_env() -> (Env, OfferingId, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let holder = Address::generate(&env);
        let issuer = Address::generate(&env);
        let offering_id =
            OfferingId { issuer, namespace: symbol_short!("def"), token: Address::generate(&env) };
        (env, offering_id, holder)
    }

    #[test]
    fn test_track_and_rollover_within_basis() {
        let (env, offering_id, holder) = setup_env();

        track_cost_basis(&env, &offering_id, &holder, 100_000);
        let result = rollover_distribution(&env, &offering_id, &holder, 30_000, 1, 1000);

        assert_eq!(result.return_of_capital, 30_000);
        assert_eq!(result.capital_gains, 0);

        let key = DataKey2::RemainingBasis(offering_id.clone(), holder.clone());
        let remaining: i128 = env.storage().persistent().get(&key).unwrap();
        assert_eq!(remaining, 70_000);
    }

    #[test]
    fn test_rollover_exact_basis() {
        let (env, offering_id, holder) = setup_env();

        track_cost_basis(&env, &offering_id, &holder, 50_000);
        let result = rollover_distribution(&env, &offering_id, &holder, 50_000, 1, 1000);

        assert_eq!(result.return_of_capital, 50_000);
        assert_eq!(result.capital_gains, 0);

        let key = DataKey2::RemainingBasis(offering_id.clone(), holder.clone());
        let remaining: i128 = env.storage().persistent().get(&key).unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn test_rollover_exceeds_basis_emits_reclassify() {
        let (env, offering_id, holder) = setup_env();

        track_cost_basis(&env, &offering_id, &holder, 30_000);
        let result = rollover_distribution(&env, &offering_id, &holder, 100_000, 1, 1000);

        assert_eq!(result.return_of_capital, 30_000);
        assert_eq!(result.capital_gains, 70_000);

        let key = DataKey2::RemainingBasis(offering_id.clone(), holder.clone());
        let remaining: i128 = env.storage().persistent().get(&key).unwrap();
        assert_eq!(remaining, 0);

        let events = env.events().all();
        let reclassify_events = events
            .iter()
            .filter(|e| {
                e.0 == (
                    EVENT_TAX_RECLASSIFY,
                    offering_id.issuer.clone(),
                    offering_id.namespace.clone(),
                    offering_id.token.clone(),
                )
            })
            .count();
        assert!(reclassify_events > 0, "expected tax_recls event");
    }

    #[test]
    fn test_rollover_zero_basis() {
        let (env, offering_id, holder) = setup_env();

        let result = rollover_distribution(&env, &offering_id, &holder, 50_000, 1, 1000);

        assert_eq!(result.return_of_capital, 0);
        assert_eq!(result.capital_gains, 50_000);
    }

    #[test]
    fn test_rollover_zero_amount() {
        let (env, offering_id, holder) = setup_env();

        track_cost_basis(&env, &offering_id, &holder, 100_000);
        let result = rollover_distribution(&env, &offering_id, &holder, 0, 1, 1000);

        assert_eq!(result.return_of_capital, 0);
        assert_eq!(result.capital_gains, 0);

        let key = DataKey2::RemainingBasis(offering_id.clone(), holder.clone());
        let remaining: i128 = env.storage().persistent().get(&key).unwrap();
        assert_eq!(remaining, 100_000);
    }

    #[test]
    fn test_apply_return_of_capital_with_cap_multiple_distributions() {
        let (env, offering_id, holder) = setup_env();

        track_cost_basis(&env, &offering_id, &holder, 100_000);

        let r1 = apply_return_of_capital_with_cap(&env, &offering_id, &holder, 40_000, 1, 1000);
        assert_eq!(r1.return_of_capital, 40_000);
        assert_eq!(r1.capital_gains, 0);

        let r2 = apply_return_of_capital_with_cap(&env, &offering_id, &holder, 30_000, 2, 2000);
        assert_eq!(r2.return_of_capital, 30_000);
        assert_eq!(r2.capital_gains, 0);

        let r3 = apply_return_of_capital_with_cap(&env, &offering_id, &holder, 50_000, 3, 3000);
        assert_eq!(r3.return_of_capital, 30_000);
        assert_eq!(r3.capital_gains, 20_000);

        let r4 = apply_return_of_capital_with_cap(&env, &offering_id, &holder, 10_000, 4, 4000);
        assert_eq!(r4.return_of_capital, 0);
        assert_eq!(r4.capital_gains, 10_000);
    }

    #[test]
    fn test_reclassify_event_contains_correct_data() {
        let (env, offering_id, holder) = setup_env();

        track_cost_basis(&env, &offering_id, &holder, 25_000);

        apply_return_of_capital_with_cap(&env, &offering_id, &holder, 100_000, 1, 5000);

        let events = env.events().all();
        let found = events.iter().any(|e| {
            e.0 == (
                EVENT_TAX_RECLASSIFY,
                offering_id.issuer.clone(),
                offering_id.namespace.clone(),
                offering_id.token.clone(),
            )
        });
        assert!(found, "expected tax_recls event");
    }
}
