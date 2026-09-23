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
//! # Time-window tracking for price validity (issue #735)
//!
//! Every price submission now carries an explicit validity window:
//!
//! ```text
//! valid_from ──────────── valid_until
//!     │                      │
//!  earliest ledger ts     expiry ts (exclusive)
//!  at which the price     after which the price
//!  is considered fresh    is considered stale
//! ```
//!
//! ## Design
//!
//! **`valid_from`** is the observation timestamp supplied by the submitter
//! (previously called `timestamp`; still the storage key component).
//!
//! **`valid_until`** is `valid_from + validity_window_secs`.  The caller
//! passes `validity_window_secs` at submission time.  A positive, non-zero
//! window is required; zero or overflow are rejected with
//! [`Error::InvalidWindow`].
//!
//! **`DEFAULT_VALIDITY_WINDOW_SECS`** (86 400 s = 24 h) is the recommended
//! window for daily cost-of-living price observations.  Callers may pass a
//! different value but it must still be positive.
//!
//! ## Expiry checks
//!
//! [`PriceVault::is_valid_at`] is the canonical expiry predicate.  It
//! returns `true` iff `valid_from <= query_ts < valid_until`.  The upper
//! bound is exclusive so two consecutive non-overlapping windows
//! `[t, t+w)` and `[t+w, t+2w)` share no overlap.
//!
//! The `KovaraIndex` aggregation logic — and any sentinel daemon — must
//! call `is_valid_at` (or perform equivalent bounds checks) before
//! incorporating a price submission into an index update, to satisfy the
//! "expired entries are excluded" requirement.
//!
//! [`PriceVault::get_valid`] combines `get` and `is_valid_at`: it loads a
//! submission and returns `Some(submission)` only if it is currently fresh,
//! `None` if it has expired or was never recorded.  This is the recommended
//! call for consumers that want a single authoritative "is this price
//! usable?" answer.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default validity window: 24 hours expressed in seconds.
///
/// A price observation submitted without an explicit window request should
/// use this default.  24 h is appropriate for daily cost-of-living basket
/// items (bread, rent, transport) where intra-day volatility is minimal.
pub const DEFAULT_VALIDITY_WINDOW_SECS: u64 = 86_400;

// ── Error codes ───────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// The provided `valid_from` timestamp is zero or otherwise unusable.
    InvalidTimestamp = 1,
    /// `validity_window_secs` is zero, or `valid_from + window` would
    /// overflow `u64`.
    InvalidWindow = 2,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// `(country_iso, category, valid_from)` → [`PriceSubmission`].
    ///
    /// The storage key uses `valid_from` (the observation timestamp) to
    /// match the pre-existing scheme and remain backward-compatible with
    /// any index built on that triple.
    Price(Symbol, Symbol, u64),
}

// ── Domain types ──────────────────────────────────────────────────────────────

/// A single raw price submission with an attached validity window.
///
/// The window `[valid_from, valid_until)` defines the interval during which
/// this submission is considered a fresh price observation.  After
/// `valid_until` the record remains in persistent storage (for historical
/// audit) but is excluded from any live aggregation.
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
    pub value: i128,
    /// The ledger timestamp at which this observation was made.
    ///
    /// Also the start of the validity window and the storage key component.
    pub valid_from: u64,
    /// The ledger timestamp after which this observation is considered stale
    /// (`valid_from + validity_window_secs`, exclusive upper bound).
    ///
    /// Once the current ledger timestamp reaches or exceeds `valid_until`,
    /// `is_valid_at` returns `false` and `get_valid` returns `None`.
    pub valid_until: u64,
}

impl PriceSubmission {
    /// Returns `true` iff `query_ts` falls within `[valid_from, valid_until)`.
    ///
    /// The lower bound is inclusive (a submission is immediately valid at the
    /// moment it is observed).  The upper bound is exclusive so consecutive
    /// non-overlapping windows share no ambiguous boundary.
    #[inline]
    pub fn is_valid_at(&self, query_ts: u64) -> bool {
        query_ts >= self.valid_from && query_ts < self.valid_until
    }
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct PriceVault;

#[contractimpl]
impl PriceVault {
    // ── Mutating entry points ─────────────────────────────────────────────

    /// Record a raw price submission with an explicit validity window.
    ///
    /// `valid_from` is the observation timestamp (storage key component).
    /// `validity_window_secs` defines how long the submission is considered
    /// fresh.  [`DEFAULT_VALIDITY_WINDOW_SECS`] (86 400 s) is recommended
    /// for daily basket observations.
    ///
    /// # Errors
    /// * [`Error::InvalidTimestamp`] — `valid_from` is zero.
    /// * [`Error::InvalidWindow`] — `validity_window_secs` is zero, or
    ///   `valid_from + validity_window_secs` overflows `u64`.
    pub fn submit(
        env: Env,
        submitter: Address,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        valid_from: u64,
        validity_window_secs: u64,
    ) -> Result<(), Error> {
        if valid_from == 0 {
            return Err(Error::InvalidTimestamp);
        }
        if validity_window_secs == 0 {
            return Err(Error::InvalidWindow);
        }
        let valid_until = valid_from
            .checked_add(validity_window_secs)
            .ok_or(Error::InvalidWindow)?;

        submitter.require_auth();

        env.storage().persistent().set(
            &DataKey::Price(country_iso.clone(), category.clone(), valid_from),
            &PriceSubmission {
                submitter,
                country_iso,
                category,
                value,
                valid_from,
                valid_until,
            },
        );
        Ok(())
    }

    // ── Read entry points ─────────────────────────────────────────────────

    /// Read a stored price submission by its composite key, if present.
    ///
    /// Returns the record regardless of whether it is currently within its
    /// validity window.  Use [`Self::get_valid`] to gate on freshness.
    pub fn get(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        valid_from: u64,
    ) -> Option<PriceSubmission> {
        env.storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, valid_from))
    }

    /// Read a stored price submission only if it is still within its
    /// validity window at `query_ts`.
    ///
    /// Returns `None` when:
    /// - no submission exists for the given key, **or**
    /// - the submission exists but `query_ts >= valid_until` (expired), **or**
    /// - `query_ts < valid_from` (not yet valid).
    ///
    /// This is the recommended call for aggregation and index calculation
    /// code that must exclude stale prices.
    pub fn get_valid(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        valid_from: u64,
        query_ts: u64,
    ) -> Option<PriceSubmission> {
        let submission: PriceSubmission = env
            .storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, valid_from))?;

        if submission.is_valid_at(query_ts) {
            Some(submission)
        } else {
            None
        }
    }

    /// Check whether a stored submission is valid at the given timestamp.
    ///
    /// Returns `false` for missing submissions (rather than an error) so
    /// callers can use a simple boolean gate without matching on `Option`.
    pub fn is_valid_at(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        valid_from: u64,
        query_ts: u64,
    ) -> bool {
        let submission: Option<PriceSubmission> = env
            .storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, valid_from));

        match submission {
            Some(s) => s.is_valid_at(query_ts),
            None => false,
        }
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

    // ── submit validation ─────────────────────────────────────────────────

    #[test]
    fn submit_zero_valid_from_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &1000,
                &0_u64,
                &DEFAULT_VALIDITY_WINDOW_SECS,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTimestamp);
    }

    #[test]
    fn submit_zero_window_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &1000,
                &1000_u64,
                &0_u64,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidWindow);
    }

    #[test]
    fn submit_overflow_window_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &1000,
                &u64::MAX,      // valid_from
                &1_u64,         // valid_from + 1 overflows u64
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidWindow);
    }

    #[test]
    fn submit_stores_correct_window_bounds() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client
            .submit(&submitter, &country, &category, &1000, &1_000_u64, &DEFAULT_VALIDITY_WINDOW_SECS)
            .unwrap();

        let s = client.get(&country, &category, &1_000_u64).unwrap();
        assert_eq!(s.valid_from, 1_000);
        assert_eq!(s.valid_until, 1_000 + DEFAULT_VALIDITY_WINDOW_SECS);
    }

    // ── is_valid_at (method on PriceSubmission) ───────────────────────────

    #[test]
    fn is_valid_at_true_at_lower_bound() {
        let sub = PriceSubmission {
            submitter: soroban_sdk::Address::from_str(
                &Env::default(),
                "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN",
            ),
            country_iso: Symbol::new(&Env::default(), "NG"),
            category: Symbol::new(&Env::default(), "Food"),
            value: 100,
            valid_from: 1_000,
            valid_until: 2_000,
        };
        assert!(sub.is_valid_at(1_000));
    }

    #[test]
    fn is_valid_at_false_at_upper_bound_exclusive() {
        let env = Env::default();
        let sub = PriceSubmission {
            submitter: Address::generate(&env),
            country_iso: sym(&env, "NG"),
            category: sym(&env, "Food"),
            value: 100,
            valid_from: 1_000,
            valid_until: 2_000,
        };
        // Upper bound is exclusive.
        assert!(!sub.is_valid_at(2_000));
    }

    #[test]
    fn is_valid_at_false_before_window() {
        let env = Env::default();
        let sub = PriceSubmission {
            submitter: Address::generate(&env),
            country_iso: sym(&env, "NG"),
            category: sym(&env, "Food"),
            value: 100,
            valid_from: 1_000,
            valid_until: 2_000,
        };
        assert!(!sub.is_valid_at(999));
    }

    #[test]
    fn is_valid_at_true_inside_window() {
        let env = Env::default();
        let sub = PriceSubmission {
            submitter: Address::generate(&env),
            country_iso: sym(&env, "NG"),
            category: sym(&env, "Food"),
            value: 100,
            valid_from: 1_000,
            valid_until: 2_000,
        };
        assert!(sub.is_valid_at(1_500));
    }

    // ── contract is_valid_at ──────────────────────────────────────────────

    #[test]
    fn contract_is_valid_at_false_for_missing() {
        let env = make_env();
        let client = deploy(&env);
        let result =
            client.is_valid_at(&sym(&env, "NG"), &sym(&env, "Food"), &1_000_u64, &1_000_u64);
        assert!(!result);
    }

    #[test]
    fn contract_is_valid_at_true_inside_window() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100,
                &1_000_u64,
                &86_400_u64,
            )
            .unwrap();

        assert!(client.is_valid_at(&sym(&env, "NG"), &sym(&env, "Food"), &1_000_u64, &50_000_u64));
    }

    #[test]
    fn contract_is_valid_at_false_after_expiry() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100,
                &1_000_u64,
                &3_600_u64, // 1 hour window
            )
            .unwrap();

        // Query at valid_until (4600) — exclusive upper bound, so invalid.
        assert!(!client.is_valid_at(
            &sym(&env, "NG"),
            &sym(&env, "Food"),
            &1_000_u64,
            &4_600_u64
        ));
    }

    // ── get_valid ─────────────────────────────────────────────────────────

    #[test]
    fn get_valid_returns_none_for_missing() {
        let env = make_env();
        let client = deploy(&env);
        assert!(client
            .get_valid(&sym(&env, "NG"), &sym(&env, "Food"), &1_u64, &1_u64)
            .is_none());
    }

    #[test]
    fn get_valid_returns_some_inside_window() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(
                &submitter,
                &sym(&env, "KE"),
                &sym(&env, "Rent"),
                &500,
                &2_000_u64,
                &86_400_u64,
            )
            .unwrap();

        let result = client.get_valid(
            &sym(&env, "KE"),
            &sym(&env, "Rent"),
            &2_000_u64,
            &2_000_u64,
        );
        assert!(result.is_some());
        assert_eq!(result.unwrap().value, 500);
    }

    #[test]
    fn get_valid_returns_none_after_expiry() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(
                &submitter,
                &sym(&env, "BR"),
                &sym(&env, "Transport"),
                &300,
                &1_000_u64,
                &3_600_u64, // expires at 4600
            )
            .unwrap();

        // Query well past expiry.
        let result = client.get_valid(
            &sym(&env, "BR"),
            &sym(&env, "Transport"),
            &1_000_u64,
            &100_000_u64,
        );
        assert!(result.is_none());
    }

    #[test]
    fn get_valid_returns_none_before_valid_from() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(
                &submitter,
                &sym(&env, "IN"),
                &sym(&env, "Health"),
                &200,
                &5_000_u64,
                &86_400_u64,
            )
            .unwrap();

        // Query before the window starts.
        let result = client.get_valid(
            &sym(&env, "IN"),
            &sym(&env, "Health"),
            &5_000_u64,
            &4_999_u64,
        );
        assert!(result.is_none());
    }

    // ── consecutive non-overlapping windows ───────────────────────────────

    #[test]
    fn consecutive_windows_do_not_overlap() {
        // Window 1: [1000, 2000)
        // Window 2: [2000, 3000)
        // query_ts=2000 must match only window 2.
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        // Window 1
        client
            .submit(&submitter, &country, &category, &100, &1_000_u64, &1_000_u64)
            .unwrap();
        // Window 2
        client
            .submit(&submitter, &country, &category, &200, &2_000_u64, &1_000_u64)
            .unwrap();

        // At ts=2000: window 1 has expired (upper bound exclusive), window 2 is fresh.
        assert!(!client.is_valid_at(&country, &category, &1_000_u64, &2_000_u64));
        assert!(client.is_valid_at(&country, &category, &2_000_u64, &2_000_u64));
    }

    // ── default window constant ───────────────────────────────────────────

    #[test]
    fn default_validity_window_is_24_hours() {
        assert_eq!(DEFAULT_VALIDITY_WINDOW_SECS, 86_400);
    }
}
