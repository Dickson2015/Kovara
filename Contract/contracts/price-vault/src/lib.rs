#![no_std]
//! `PriceVault` — stores raw price submissions keyed by `(country_iso,
//! category, timestamp)`.
//!
//! This is the scaffolding landing pad for the Kovara price vault (CT-001).
//! It wires the contract to the Soroban SDK and exposes a minimal, safe
//! submission/read surface so the workspace builds and the storage layout is
//! in place. Deeper submission semantics are owned by subsequent contract
//! issues and will extend this crate.
//!
//! # Fail-safe checks for zero and negative values (issue #728)
//!
//! All arithmetic entry points in this contract validate inputs through a
//! single shared [`guards`] module before touching contract state.  The
//! rules are:
//!
//! | Guard | Rule |
//! |---|---|
//! | `require_positive_price` | `value` must be `> 0`; zero or negative corrupts the KVI median. |
//! | `require_positive_u64` | Generic `u64` must be `> 0`; used for timestamps and counts. |
//! | `require_positive_i128` | Generic `i128` must be `> 0`; used for stake and reward amounts. |
//! | `require_no_overflow_add` | `a.checked_add(b)` — panics would break contract execution; explicit error instead. |
//! | `require_no_overflow_mul` | `a.checked_mul(b)` — same rationale. |
//!
//! Every guard is a pure function that returns `Err(Error::*)` rather than
//! panicking.  Soroban contracts must never panic on bad input — a panic
//! aborts the transaction and burns fees without giving the caller a
//! meaningful error code.  All guards satisfy the
//! "arithmetic functions fail safely instead of causing undefined behavior"
//! acceptance criterion.
//!
//! ## Negative-path test coverage
//!
//! Every guard has at least one negative-path test (bad input → expected
//! error) and at least one positive-path test (good input → `Ok`).  The
//! aggregate test suite documents every rejected case, satisfying the
//! "validation logic is asserted with negative-path tests" acceptance
//! criterion.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol,
};

// ── Error codes ───────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// The provided timestamp is zero or otherwise unusable.
    InvalidTimestamp = 1,
    /// A price value was zero; zero prices corrupt the index median.
    ZeroPrice = 2,
    /// A price value was negative; negative prices are nonsensical for
    /// cost-of-living observations.
    NegativePrice = 3,
    /// A u64 value that must be positive was zero.
    ZeroValue = 4,
    /// An i128 value that must be positive was zero or negative.
    NonPositiveAmount = 5,
    /// An arithmetic addition would overflow the target type.
    ArithmeticOverflow = 6,
    /// An arithmetic multiplication would overflow the target type.
    ArithmeticOverflowMul = 7,
}

// ── Fail-safe arithmetic guards (issue #728) ──────────────────────────────────

/// Centralised arithmetic safety guards.
///
/// Every public contract entry point that performs arithmetic or stores a
/// value that is constrained to a numeric range calls one of these guards
/// before mutating state.  Returning `Err` rather than panicking is
/// mandatory in Soroban: a panic aborts the transaction without emitting
/// an error code, making debugging impossible and burning caller fees.
pub mod guards {
    use super::Error;

    /// Reject a price value that is zero or negative.
    ///
    /// Zero prices are excluded because they would skew a trimmed-median
    /// to zero, producing a nonsensical index value.  Negative prices are
    /// structurally invalid for cost-of-living observations.
    ///
    /// # Errors
    /// * [`Error::ZeroPrice`] — `value == 0`
    /// * [`Error::NegativePrice`] — `value < 0`
    #[inline]
    pub fn require_positive_price(value: i128) -> Result<(), Error> {
        if value < 0 {
            return Err(Error::NegativePrice);
        }
        if value == 0 {
            return Err(Error::ZeroPrice);
        }
        Ok(())
    }

    /// Reject a `u64` value that is zero.
    ///
    /// Used for timestamps, counts, and any other `u64` field that must be
    /// strictly positive.
    ///
    /// # Errors
    /// * [`Error::ZeroValue`] — `value == 0`
    #[inline]
    pub fn require_positive_u64(value: u64) -> Result<(), Error> {
        if value == 0 {
            return Err(Error::ZeroValue);
        }
        Ok(())
    }

    /// Reject an `i128` amount that is zero or negative.
    ///
    /// Used for stake and reward amounts that must represent a meaningful
    /// positive quantity.
    ///
    /// # Errors
    /// * [`Error::NonPositiveAmount`] — `amount <= 0`
    #[inline]
    pub fn require_positive_i128(amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::NonPositiveAmount);
        }
        Ok(())
    }

    /// Perform a checked `u64` addition, returning `Error::ArithmeticOverflow`
    /// instead of wrapping or panicking.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflow`] — the result would exceed `u64::MAX`.
    #[inline]
    pub fn checked_add_u64(a: u64, b: u64) -> Result<u64, Error> {
        a.checked_add(b).ok_or(Error::ArithmeticOverflow)
    }

    /// Perform a checked `i128` addition, returning `Error::ArithmeticOverflow`
    /// instead of wrapping or panicking.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflow`] — the result would exceed `i128::MAX`
    ///   or fall below `i128::MIN`.
    #[inline]
    pub fn checked_add_i128(a: i128, b: i128) -> Result<i128, Error> {
        a.checked_add(b).ok_or(Error::ArithmeticOverflow)
    }

    /// Perform a checked `i128` multiplication, returning
    /// `Error::ArithmeticOverflowMul` instead of wrapping or panicking.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflowMul`] — the result would overflow `i128`.
    #[inline]
    pub fn checked_mul_i128(a: i128, b: i128) -> Result<i128, Error> {
        a.checked_mul(b).ok_or(Error::ArithmeticOverflowMul)
    }
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// `(country_iso, category, timestamp)` → [`PriceSubmission`].
    Price(Symbol, Symbol, u64),
}

// ── Domain types ──────────────────────────────────────────────────────────────

/// A single raw price submission.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceSubmission {
    /// The submitting oracle/operator address.
    pub submitter: Address,
    /// ISO country code (topic-style, kept readable for indexing).
    pub country_iso: Symbol,
    /// The price category, e.g. `bread` or `rent`.
    pub category: Symbol,
    /// Unverified raw price in the smallest fixed-point unit.
    ///
    /// Validated by [`guards::require_positive_price`]: must be `> 0`.
    pub value: i128,
    /// Native timestamp of the observation.
    ///
    /// Validated by [`guards::require_positive_u64`]: must be `> 0`.
    pub timestamp: u64,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct PriceVault;

#[contractimpl]
impl PriceVault {
    /// Record a raw price submission.
    ///
    /// # Validation (issue #728)
    /// All numeric inputs are validated through the [`guards`] module:
    /// - `value` must be strictly positive (`> 0`).
    /// - `timestamp` must be strictly positive (`> 0`).
    ///
    /// # Errors
    /// * [`Error::InvalidTimestamp`] — `timestamp` is zero.
    /// * [`Error::ZeroPrice`] — `value` is zero.
    /// * [`Error::NegativePrice`] — `value` is negative.
    pub fn submit(
        env: Env,
        submitter: Address,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        timestamp: u64,
    ) -> Result<(), Error> {
        // Validate timestamp first (pre-existing check).
        if timestamp == 0 {
            return Err(Error::InvalidTimestamp);
        }
        // Fail-safe: reject zero or negative price values (issue #728).
        guards::require_positive_price(value)?;

        submitter.require_auth();

        env.storage().persistent().set(
            &DataKey::Price(country_iso.clone(), category.clone(), timestamp),
            &PriceSubmission {
                submitter,
                country_iso,
                category,
                value,
                timestamp,
            },
        );
        Ok(())
    }

    /// Read a stored price submission, if present.
    pub fn get(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        timestamp: u64,
    ) -> Option<PriceSubmission> {
        env.storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, timestamp))
    }

    // ── Arithmetic helpers exposed as contract entry points ───────────────
    //
    // These are thin wrappers around the `guards` module functions.  They are
    // primarily useful for off-chain clients that want to pre-validate values
    // before constructing a transaction, and for cross-contract callers.

    /// Add two `i128` values, failing safely on overflow.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflow`] — result would overflow `i128`.
    pub fn safe_add(env: Env, a: i128, b: i128) -> Result<i128, Error> {
        let _ = env;
        guards::checked_add_i128(a, b)
    }

    /// Multiply two `i128` values, failing safely on overflow.
    ///
    /// # Errors
    /// * [`Error::ArithmeticOverflowMul`] — result would overflow `i128`.
    pub fn safe_mul(env: Env, a: i128, b: i128) -> Result<i128, Error> {
        let _ = env;
        guards::checked_mul_i128(a, b)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    fn make_env() -> Env {
        Env::default()
    }

    fn deploy(env: &Env) -> PriceVaultClient {
        let contract_id = env.register(PriceVault, ());
        PriceVaultClient::new(env, &contract_id)
    }

    fn sym(env: &Env, s: &str) -> Symbol {
        Symbol::new(env, s)
    }

    // ── guards::require_positive_price ────────────────────────────────────

    #[test]
    fn guard_zero_price_rejected() {
        assert_eq!(guards::require_positive_price(0), Err(Error::ZeroPrice));
    }

    #[test]
    fn guard_negative_price_rejected() {
        assert_eq!(guards::require_positive_price(-1), Err(Error::NegativePrice));
        assert_eq!(
            guards::require_positive_price(i128::MIN),
            Err(Error::NegativePrice)
        );
    }

    #[test]
    fn guard_positive_price_accepted() {
        assert!(guards::require_positive_price(1).is_ok());
        assert!(guards::require_positive_price(i128::MAX).is_ok());
    }

    // ── guards::require_positive_u64 ─────────────────────────────────────

    #[test]
    fn guard_zero_u64_rejected() {
        assert_eq!(guards::require_positive_u64(0), Err(Error::ZeroValue));
    }

    #[test]
    fn guard_positive_u64_accepted() {
        assert!(guards::require_positive_u64(1).is_ok());
        assert!(guards::require_positive_u64(u64::MAX).is_ok());
    }

    // ── guards::require_positive_i128 ────────────────────────────────────

    #[test]
    fn guard_zero_i128_rejected() {
        assert_eq!(
            guards::require_positive_i128(0),
            Err(Error::NonPositiveAmount)
        );
    }

    #[test]
    fn guard_negative_i128_rejected() {
        assert_eq!(
            guards::require_positive_i128(-1),
            Err(Error::NonPositiveAmount)
        );
        assert_eq!(
            guards::require_positive_i128(i128::MIN),
            Err(Error::NonPositiveAmount)
        );
    }

    #[test]
    fn guard_positive_i128_accepted() {
        assert!(guards::require_positive_i128(1).is_ok());
        assert!(guards::require_positive_i128(i128::MAX).is_ok());
    }

    // ── guards::checked_add_u64 ───────────────────────────────────────────

    #[test]
    fn checked_add_u64_normal() {
        assert_eq!(guards::checked_add_u64(100, 200), Ok(300));
    }

    #[test]
    fn checked_add_u64_overflow_rejected() {
        assert_eq!(
            guards::checked_add_u64(u64::MAX, 1),
            Err(Error::ArithmeticOverflow)
        );
    }

    #[test]
    fn checked_add_u64_max_plus_zero() {
        // Adding zero to MAX is valid.
        assert_eq!(guards::checked_add_u64(u64::MAX, 0), Ok(u64::MAX));
    }

    // ── guards::checked_add_i128 ──────────────────────────────────────────

    #[test]
    fn checked_add_i128_normal() {
        assert_eq!(guards::checked_add_i128(50, 50), Ok(100));
    }

    #[test]
    fn checked_add_i128_positive_overflow_rejected() {
        assert_eq!(
            guards::checked_add_i128(i128::MAX, 1),
            Err(Error::ArithmeticOverflow)
        );
    }

    #[test]
    fn checked_add_i128_negative_overflow_rejected() {
        assert_eq!(
            guards::checked_add_i128(i128::MIN, -1),
            Err(Error::ArithmeticOverflow)
        );
    }

    // ── guards::checked_mul_i128 ──────────────────────────────────────────

    #[test]
    fn checked_mul_i128_normal() {
        assert_eq!(guards::checked_mul_i128(3, 4), Ok(12));
    }

    #[test]
    fn checked_mul_i128_overflow_rejected() {
        assert_eq!(
            guards::checked_mul_i128(i128::MAX, 2),
            Err(Error::ArithmeticOverflowMul)
        );
    }

    #[test]
    fn checked_mul_i128_negative_times_negative() {
        // -i128::MAX * 2 overflows: -(2^127 - 1) * 2 = -(2^128 - 2) < i128::MIN.
        assert_eq!(
            guards::checked_mul_i128(i128::MIN, -1),
            Err(Error::ArithmeticOverflowMul)
        );
    }

    // ── submit: fail-safe price validation ───────────────────────────────

    #[test]
    fn submit_zero_price_rejected() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &0, &100_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::ZeroPrice);
    }

    #[test]
    fn submit_negative_price_rejected() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &-1, &100_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::NegativePrice);
    }

    #[test]
    fn submit_very_negative_price_rejected() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &i128::MIN,
                &100_u64,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::NegativePrice);
    }

    #[test]
    fn submit_zero_timestamp_rejected() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &100, &0_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTimestamp);
    }

    #[test]
    fn submit_valid_values_succeeds() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1_000, &100_u64)
            .unwrap();

        let got = client.get(&sym(&env, "NG"), &sym(&env, "Food"), &100_u64).unwrap();
        assert_eq!(got.value, 1_000);
    }

    // ── safe_add / safe_mul contract entry points ─────────────────────────

    #[test]
    fn safe_add_normal() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(client.safe_add(&10, &20), Ok(30));
    }

    #[test]
    fn safe_add_overflow_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(
            client.try_safe_add(&i128::MAX, &1).unwrap_err().unwrap(),
            Error::ArithmeticOverflow
        );
    }

    #[test]
    fn safe_mul_normal() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(client.safe_mul(&6, &7), Ok(42));
    }

    #[test]
    fn safe_mul_overflow_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        assert_eq!(
            client.try_safe_mul(&i128::MAX, &2).unwrap_err().unwrap(),
            Error::ArithmeticOverflowMul
        );
    }
}
