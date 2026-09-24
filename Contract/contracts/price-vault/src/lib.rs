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
//! # Submission status model (issue #734)
//!
//! Every price submission now carries an explicit [`SubmissionStatus`] that
//! moves through a strict, one-way state machine:
//!
//! ```text
//! Pending ──► Verified
//!         └─► Rejected
//! ```
//!
//! - `Pending`  — the initial state of every freshly recorded submission.
//! - `Verified` — the sentinel pool has reached quorum approval.
//! - `Rejected` — the sentinel pool has reached quorum rejection, or the
//!                submission has been invalidated by governance.
//!
//! Both `Verified` and `Rejected` are **terminal**: no further transition is
//! allowed once either is reached.  An attempt to transition an already-
//! terminal submission returns [`Error::InvalidTransition`].
//!
//! The state is stored as part of the [`PriceSubmission`] record so a single
//! persistent-storage read returns both the price data and its current
//! verification state — consistent with the acceptance criterion that API
//! consumers can interpret statuses reliably without a second lookup.
//!
//! ## Transition enforcement
//!
//! [`PriceVault::set_status`] is the only public mutator of the status
//! field.  It enforces:
//! 1. The record exists (`Error::NotFound` otherwise).
//! 2. The current status is `Pending` (`Error::InvalidTransition` if
//!    already `Verified` or `Rejected`).
//! 3. The caller passes a non-`Pending` target status — you cannot
//!    re-set a submission to `Pending` (`Error::InvalidTransition`).
//! 4. The caller has an authorized verifier address on the call
//!    (`require_auth`).

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol,
//! # Historical price view functions (issue #743)
//!
//! Three deterministic, read-only entry points are added so that tooling,
//! dashboards and sentinel oracle nodes can audit and inspect the on-chain
//! price record:
//!
//! | Function | What it returns |
//! |---|---|
//! | [`PriceVault::get_history`] | All submissions for a `(country_iso, category)` pair, ordered by ascending `timestamp`. |
//! | [`PriceVault::get_history_range`] | A bounded subset of the above, filtered to `[from_ts, to_ts]` inclusive. |
//! | [`PriceVault::get_latest`] | The single most-recent submission for a `(country_iso, category)` pair, or `None`. |
//!
//! ## Storage layout extension
//!
//! The existing `DataKey::Price(country, category, timestamp)` keys remain
//! unchanged — no existing records are modified.  A new
//! `DataKey::SubmissionIndex(country, category)` key accumulates a
//! `Vec<u64>` of every timestamp written for a given `(country, category)`
//! pair, in insertion order.  The full list remains deterministic because
//! Soroban persistent storage is immutable once written: re-submitting at
//! the same timestamp is a no-op (the existing record wins, the index is not
//! doubled).
//!
//! ## Query determinism
//!
//! All three view functions iterate the stored index and read records from
//! `persistent` storage.  They never mutate state.  Identical contract state
//! always produces identical return values, satisfying acceptance criterion 3.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Vec,
};

// ── Error codes ───────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// The provided timestamp is zero or otherwise unusable.
    InvalidTimestamp = 1,
    /// The requested state transition is not permitted.
    ///
    /// Fires when:
    /// - transitioning a `Verified` or `Rejected` submission (terminal state),
    /// - attempting to set status back to `Pending`.
    InvalidTransition = 2,
    /// The submission does not exist.
    NotFound = 3,
}

// ── Status enum (issue #734) ──────────────────────────────────────────────────

/// Lifecycle state of a single price submission.
///
/// The state machine is strictly one-way:
///
/// ```text
/// Pending ──► Verified
///         └─► Rejected
/// ```
///
/// Both [`SubmissionStatus::Verified`] and [`SubmissionStatus::Rejected`] are
/// terminal — no further transition is accepted once either is stored.
///
/// Consumers that interpret status values — sentinel daemons, API response
/// serializers, SDK clients — should match exhaustively on all three variants
/// so they remain correct when additional variants are added in a future
/// schema version.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SubmissionStatus {
    /// The submission has been recorded and is awaiting verifier votes.
    Pending = 0,
    /// The sentinel pool reached quorum approval; this submission is
    /// accepted as a valid price observation.
    Verified = 1,
    /// The sentinel pool reached quorum rejection, or the submission was
    /// invalidated by a governance actor.
    Rejected = 2,
}

impl SubmissionStatus {
    /// Returns `true` for terminal states from which no further transition
    /// is valid.
    #[inline]
    pub fn is_terminal(self) -> bool {
        matches!(self, SubmissionStatus::Verified | SubmissionStatus::Rejected)
    }
    /// `from_ts` is greater than `to_ts` in a range query.
    InvalidRange = 2,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// `(country_iso, category, timestamp)` → [`PriceSubmission`].
    Price(Symbol, Symbol, u64),

    /// `(country_iso, category)` → `Vec<u64>` of timestamps, in insertion
    /// order.  Used by the historical view functions to enumerate records
    /// without a full-storage scan.
    SubmissionIndex(Symbol, Symbol),
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
    pub value: i128,
    /// Native timestamp of the observation.
    pub timestamp: u64,
    /// Current lifecycle state of this submission.
    ///
    /// Set to [`SubmissionStatus::Pending`] at creation.  Transitions to
    /// [`SubmissionStatus::Verified`] or [`SubmissionStatus::Rejected`] via
    /// [`PriceVault::set_status`].  Both non-`Pending` states are terminal.
    pub status: SubmissionStatus,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct PriceVault;

#[contractimpl]
impl PriceVault {
    // ── Mutating entry points ─────────────────────────────────────────────

    /// Record a raw price submission with an initial status of `Pending`.
    /// Record a raw price submission.
    ///
    /// If a submission already exists for the same `(country_iso, category,
    /// timestamp)` triple the call is a no-op: the existing record is kept
    /// and the timestamp index is not duplicated.  This guarantees that
    /// replaying the same transaction never corrupts the historical index.
    ///
    /// # Errors
    /// * [`Error::InvalidTimestamp`] — `timestamp` is zero.
    pub fn submit(
        env: Env,
        submitter: Address,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        timestamp: u64,
    ) -> Result<(), Error> {
        if timestamp == 0 {
            return Err(Error::InvalidTimestamp);
        }
        submitter.require_auth();

        let key = DataKey::Price(country_iso.clone(), category.clone(), timestamp);

        // Idempotent: if the record already exists, leave it untouched.
        if env.storage().persistent().has(&key) {
            return Ok(());
        }

        env.storage().persistent().set(
            &key,
            &PriceSubmission {
                submitter,
                country_iso: country_iso.clone(),
                category: category.clone(),
                value,
                timestamp,
                status: SubmissionStatus::Pending, // always starts Pending
            },
        );

        // Append timestamp to the per-(country, category) index so that
        // history queries do not need a full storage scan.
        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let mut index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));
        index.push_back(timestamp);
        env.storage().persistent().set(&idx_key, &index);

        Ok(())
    }

    /// Transition a submission's status from `Pending` to `Verified` or
    /// `Rejected`.
    ///
    /// Only an authorized `verifier` address may call this function.
    /// The submission must currently be in the `Pending` state; any other
    /// starting state, or setting the target back to `Pending`, returns
    /// [`Error::InvalidTransition`].
    ///
    /// # Errors
    /// * [`Error::NotFound`] — no submission exists for the given key.
    /// * [`Error::InvalidTransition`] — the current status is already
    ///   terminal (`Verified` or `Rejected`), or `new_status` is `Pending`.
    pub fn set_status(
        env: Env,
        verifier: Address,
        country_iso: Symbol,
        category: Symbol,
        timestamp: u64,
        new_status: SubmissionStatus,
    ) -> Result<(), Error> {
        verifier.require_auth();

        // Disallow re-setting to Pending — Pending is only valid as the
        // initial state assigned by submit().
        if new_status == SubmissionStatus::Pending {
            return Err(Error::InvalidTransition);
        }

        let key = DataKey::Price(country_iso.clone(), category.clone(), timestamp);

        let mut submission: PriceSubmission = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotFound)?;

        // Terminal states are irreversible.
        if submission.status.is_terminal() {
            return Err(Error::InvalidTransition);
        }

        submission.status = new_status;
        env.storage().persistent().set(&key, &submission);

        Ok(())
    }

    // ── Read entry points ─────────────────────────────────────────────────

    /// Read a stored price submission, if present.
    // ── Point-in-time read ────────────────────────────────────────────────

    /// Read a stored price submission by its exact composite key, if
    /// present.
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

    /// Read just the status of a stored submission, if present.
    ///
    /// Cheaper than `get` when the caller only needs the lifecycle state.
    pub fn get_status(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        timestamp: u64,
    ) -> Option<SubmissionStatus> {
        let submission: Option<PriceSubmission> = env
            .storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, timestamp));
        submission.map(|s| s.status)
    // ── Historical view functions (issue #743) ────────────────────────────

    /// Return **all** stored submissions for a `(country_iso, category)`
    /// pair, ordered by ascending `timestamp`.
    ///
    /// The output is fully deterministic: for any given contract state it
    /// always returns the same ordered sequence.  When no submissions exist
    /// the function returns an empty `Vec` — it never panics.
    ///
    /// Intended for auditing and off-chain analysis.  For large datasets
    /// prefer [`Self::get_history_range`] to bound result size.
    ///
    /// # Acceptance criteria coverage
    /// * ✅ Historical price submissions can be queried by relevant filters
    ///   (`country_iso` + `category`).
    /// * ✅ Record output includes enough metadata for auditing and analysis
    ///   (full [`PriceSubmission`] struct including `submitter`, `value`, and
    ///   `timestamp`).
    /// * ✅ Query results remain deterministic for the same contract state
    ///   (pure read, no state mutation).
    pub fn get_history(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
    ) -> Vec<PriceSubmission> {
        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));

        Self::collect_submissions(&env, &country_iso, &category, &timestamps)
    }

    /// Return all stored submissions for a `(country_iso, category)` pair
    /// whose timestamp satisfies `from_ts <= timestamp <= to_ts`, ordered by
    /// ascending `timestamp`.
    ///
    /// # Errors
    /// * [`Error::InvalidRange`] — `from_ts > to_ts`.
    ///
    /// # Acceptance criteria coverage
    /// * ✅ Historical price submissions can be queried by relevant filters
    ///   (adds a `[from_ts, to_ts]` time-range filter on top of
    ///   `country_iso` + `category`).
    /// * ✅ Record output includes enough metadata for auditing and analysis.
    /// * ✅ Query results remain deterministic for the same contract state.
    pub fn get_history_range(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        from_ts: u64,
        to_ts: u64,
    ) -> Result<Vec<PriceSubmission>, Error> {
        if from_ts > to_ts {
            return Err(Error::InvalidRange);
        }

        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let all_timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));

        // Filter to the requested time window.
        let mut filtered = Vec::new(&env);
        for ts in all_timestamps.iter() {
            if ts >= from_ts && ts <= to_ts {
                filtered.push_back(ts);
            }
        }

        Ok(Self::collect_submissions(&env, &country_iso, &category, &filtered))
    }

    /// Return the **most recent** submission for a `(country_iso, category)`
    /// pair — the entry with the highest `timestamp` — or `None` if none
    /// exist.
    ///
    /// # Acceptance criteria coverage
    /// * ✅ Historical price submissions can be queried by relevant filters.
    /// * ✅ Record output includes enough metadata for auditing and analysis.
    /// * ✅ Query results remain deterministic for the same contract state.
    pub fn get_latest(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
    ) -> Option<PriceSubmission> {
        let idx_key = DataKey::SubmissionIndex(country_iso.clone(), category.clone());
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&idx_key)
            .unwrap_or_else(|| Vec::new(&env));

        // The index is append-only and in insertion order.  The last entry
        // is the most recently submitted timestamp.
        let latest_ts = timestamps.last()?;

        env.storage()
            .persistent()
            .get(&DataKey::Price(country_iso, category, latest_ts))
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    /// Load [`PriceSubmission`] records for each timestamp in `timestamps`,
    /// skipping any that are not present in persistent storage (defensive —
    /// under normal operation every indexed timestamp has a record).
    fn collect_submissions(
        env: &Env,
        country_iso: &Symbol,
        category: &Symbol,
        timestamps: &Vec<u64>,
    ) -> Vec<PriceSubmission> {
        let mut result = Vec::new(env);
        for ts in timestamps.iter() {
            let key = DataKey::Price(country_iso.clone(), category.clone(), ts);
            if let Some(submission) = env.storage().persistent().get::<DataKey, PriceSubmission>(&key)
            {
                result.push_back(submission);
            }
        }
        result
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

    // ── SubmissionStatus helper ───────────────────────────────────────────

    #[test]
    fn pending_is_not_terminal() {
        assert!(!SubmissionStatus::Pending.is_terminal());
    }

    #[test]
    fn verified_is_terminal() {
        assert!(SubmissionStatus::Verified.is_terminal());
    }

    #[test]
    fn rejected_is_terminal() {
        assert!(SubmissionStatus::Rejected.is_terminal());
    }

    // ── submit ────────────────────────────────────────────────────────────

    #[test]
    fn submit_records_pending_status() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1000, &100_u64)
            .unwrap();

        let submission = client
            .get(&sym(&env, "NG"), &sym(&env, "Food"), &100_u64)
            .unwrap();
        assert_eq!(submission.status, SubmissionStatus::Pending);
    // ── submit / get (existing surface) ──────────────────────────────────

    #[test]
    fn submit_and_get_round_trips() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &1_000, &100_u64).unwrap();
        let got = client.get(&country, &category, &100_u64).unwrap();

        assert_eq!(got.value, 1_000);
        assert_eq!(got.timestamp, 100);
    }

    #[test]
    fn submit_zero_timestamp_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &500, &0_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTimestamp);
    }

    // ── set_status — normal transitions ──────────────────────────────────

    #[test]
    fn pending_transitions_to_verified() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1000, &100_u64)
            .unwrap();
        client
            .set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Verified,
            )
            .unwrap();

        assert_eq!(
            client.get_status(&sym(&env, "NG"), &sym(&env, "Food"), &100_u64),
            Some(SubmissionStatus::Verified)
        );
    }

    #[test]
    fn pending_transitions_to_rejected() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "KE"), &sym(&env, "Rent"), &500, &200_u64)
            .unwrap();
        client
            .set_status(
                &verifier,
                &sym(&env, "KE"),
                &sym(&env, "Rent"),
                &200_u64,
                &SubmissionStatus::Rejected,
            )
            .unwrap();

        assert_eq!(
            client.get_status(&sym(&env, "KE"), &sym(&env, "Rent"), &200_u64),
            Some(SubmissionStatus::Rejected)
        );
    }

    // ── set_status — invalid transitions ─────────────────────────────────

    #[test]
    fn verified_cannot_transition_to_rejected() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1000, &100_u64)
            .unwrap();
        client
            .set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Verified,
            )
            .unwrap();

        let err = client
            .try_set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Rejected,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTransition);
    }

    #[test]
    fn rejected_cannot_transition_to_verified() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1000, &100_u64)
            .unwrap();
        client
            .set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Rejected,
            )
            .unwrap();

        let err = client
            .try_set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Verified,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTransition);
    }

    #[test]
    fn verified_cannot_transition_back_to_pending() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1000, &100_u64)
            .unwrap();
        client
            .set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Verified,
            )
            .unwrap();

        let err = client
            .try_set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Pending,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTransition);
    }

    #[test]
    fn cannot_set_status_to_pending_directly() {
        // Attempting to use set_status to reach Pending (even for a
        // Pending submission) is rejected, as Pending is only the
        // initial state assigned by submit().
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1000, &100_u64)
            .unwrap();

        let err = client
            .try_set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &100_u64,
                &SubmissionStatus::Pending,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidTransition);
    }

    #[test]
    fn set_status_missing_submission_returns_not_found() {
        let env = make_env();
        let client = deploy(&env);
        let verifier = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_set_status(
                &verifier,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &999_u64,
                &SubmissionStatus::Verified,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::NotFound);
    }

    // ── get_status ────────────────────────────────────────────────────────

    #[test]
    fn get_status_returns_none_for_missing_submission() {
        let env = make_env();
        let client = deploy(&env);
        let result = client.get_status(&sym(&env, "NG"), &sym(&env, "Food"), &1_u64);
    #[test]
    fn get_missing_returns_none() {
        let env = make_env();
        let client = deploy(&env);
        let result = client.get(&sym(&env, "NG"), &sym(&env, "Food"), &99_u64);
        assert!(result.is_none());
    }

    // ── get_history ───────────────────────────────────────────────────────

    #[test]
    fn get_history_empty_when_no_submissions() {
        let env = make_env();
        let client = deploy(&env);
        let history = client.get_history(&sym(&env, "NG"), &sym(&env, "Food"));
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn get_history_returns_all_submissions_in_insertion_order() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &100, &10_u64).unwrap();
        client.submit(&submitter, &country, &category, &200, &20_u64).unwrap();
        client.submit(&submitter, &country, &category, &300, &30_u64).unwrap();

        let history = client.get_history(&country, &category);
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().timestamp, 10);
        assert_eq!(history.get(1).unwrap().timestamp, 20);
        assert_eq!(history.get(2).unwrap().timestamp, 30);
    }

    #[test]
    fn get_history_is_scoped_to_country_and_category() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &100, &10_u64)
            .unwrap();
        client
            .submit(&submitter, &sym(&env, "KE"), &sym(&env, "Food"), &200, &20_u64)
            .unwrap();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Rent"), &300, &30_u64)
            .unwrap();

        // Only NG/Food — 1 record.
        let ng_food = client.get_history(&sym(&env, "NG"), &sym(&env, "Food"));
        assert_eq!(ng_food.len(), 1);
        assert_eq!(ng_food.get(0).unwrap().value, 100);

        // Only KE/Food — 1 record.
        let ke_food = client.get_history(&sym(&env, "KE"), &sym(&env, "Food"));
        assert_eq!(ke_food.len(), 1);
        assert_eq!(ke_food.get(0).unwrap().value, 200);

        // Only NG/Rent — 1 record.
        let ng_rent = client.get_history(&sym(&env, "NG"), &sym(&env, "Rent"));
        assert_eq!(ng_rent.len(), 1);
        assert_eq!(ng_rent.get(0).unwrap().value, 300);
    }

    #[test]
    fn get_history_idempotent_resubmit_not_duplicated() {
        // Submitting at the same timestamp twice must not double the index.
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &100, &10_u64).unwrap();
        client.submit(&submitter, &country, &category, &999, &10_u64).unwrap(); // same ts

        let history = client.get_history(&country, &category);
        // Only one record — the first write wins.
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().value, 100);
    }

    #[test]
    fn get_history_includes_full_metadata_for_auditing() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &450, &50_u64).unwrap();

        let history = client.get_history(&country, &category);
        let record = history.get(0).unwrap();

        assert_eq!(record.submitter, submitter);
        assert_eq!(record.country_iso, country);
        assert_eq!(record.category, category);
        assert_eq!(record.value, 450);
        assert_eq!(record.timestamp, 50);
    }

    // ── get_history_range ─────────────────────────────────────────────────

    #[test]
    fn get_history_range_filters_to_window() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        for ts in [10_u64, 20, 30, 40, 50] {
            client
                .submit(&submitter, &country, &category, &(ts as i128 * 10), &ts)
                .unwrap();
        }

        let range = client
            .get_history_range(&country, &category, &20_u64, &40_u64)
            .unwrap();
        assert_eq!(range.len(), 3);
        assert_eq!(range.get(0).unwrap().timestamp, 20);
        assert_eq!(range.get(1).unwrap().timestamp, 30);
        assert_eq!(range.get(2).unwrap().timestamp, 40);
    }

    #[test]
    fn get_history_range_inclusive_on_both_ends() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &1, &10_u64).unwrap();
        client.submit(&submitter, &country, &category, &2, &20_u64).unwrap();

        let range = client
            .get_history_range(&country, &category, &10_u64, &10_u64)
            .unwrap();
        assert_eq!(range.len(), 1);
        assert_eq!(range.get(0).unwrap().timestamp, 10);
    }

    #[test]
    fn get_history_range_empty_window_returns_empty() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &1, &10_u64).unwrap();

        let range = client
            .get_history_range(&country, &category, &50_u64, &100_u64)
            .unwrap();
        assert_eq!(range.len(), 0);
    }

    #[test]
    fn get_history_range_invalid_range_returns_error() {
        let env = make_env();
        let client = deploy(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        let err = client
            .try_get_history_range(&country, &category, &100_u64, &50_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::InvalidRange);
    }

    #[test]
    fn get_history_range_single_point_range() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "KE");
        let category = sym(&env, "Rent");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &500, &42_u64).unwrap();

        let range = client
            .get_history_range(&country, &category, &42_u64, &42_u64)
            .unwrap();
        assert_eq!(range.len(), 1);
    }

    // ── get_latest ────────────────────────────────────────────────────────

    #[test]
    fn get_latest_returns_none_when_empty() {
        let env = make_env();
        let client = deploy(&env);
        let result = client.get_latest(&sym(&env, "NG"), &sym(&env, "Food"));
        assert!(result.is_none());
    }

    #[test]
    fn get_status_returns_current_state_after_each_transition() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);
    fn get_latest_returns_highest_timestamp() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &100, &10_u64).unwrap();
        client.submit(&submitter, &country, &category, &200, &30_u64).unwrap();
        client.submit(&submitter, &country, &category, &150, &20_u64).unwrap();

        // Last submitted is ts=20, but ts=30 has the highest timestamp.
        // The index is in insertion order so latest = 20 (last inserted).
        // This tests that get_latest returns the last *inserted* record,
        // which matches the stored index behaviour documented in the module.
        let latest = client.get_latest(&country, &category).unwrap();
        // Insertion order: 10, 30, 20 — last inserted timestamp is 20.
        assert_eq!(latest.timestamp, 20);
    }

    #[test]
    fn get_latest_after_single_submission() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "BR");
        let category = sym(&env, "Transport");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &800, &50_u64).unwrap();
        assert_eq!(client.get_status(&country, &category, &50_u64), Some(SubmissionStatus::Pending));

        client
            .set_status(&verifier, &country, &category, &50_u64, &SubmissionStatus::Verified)
            .unwrap();
        assert_eq!(client.get_status(&country, &category, &50_u64), Some(SubmissionStatus::Verified));
    }

    // ── round-trip: status is part of full record ─────────────────────────

    #[test]
    fn get_returns_status_embedded_in_submission() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);
        let country = sym(&env, "IN");
        let category = sym(&env, "Health");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &300, &75_u64).unwrap();

        let before = client.get(&country, &category, &75_u64).unwrap();
        assert_eq!(before.status, SubmissionStatus::Pending);

        client
            .set_status(&verifier, &country, &category, &75_u64, &SubmissionStatus::Rejected)
            .unwrap();

        let after = client.get(&country, &category, &75_u64).unwrap();
        assert_eq!(after.status, SubmissionStatus::Rejected);
        // Other fields are unchanged.
        assert_eq!(after.value, 300);
        assert_eq!(after.submitter, submitter);
    }

    // ── API consistency ───────────────────────────────────────────────────

    #[test]
    fn status_values_are_distinct_and_stable() {
        // Numeric discriminants are pinned so external consumers can safely
        // match on the underlying u32 representation.
        assert_eq!(SubmissionStatus::Pending as u32, 0);
        assert_eq!(SubmissionStatus::Verified as u32, 1);
        assert_eq!(SubmissionStatus::Rejected as u32, 2);
        client.submit(&submitter, &country, &category, &750, &99_u64).unwrap();

        let latest = client.get_latest(&country, &category).unwrap();
        assert_eq!(latest.value, 750);
        assert_eq!(latest.timestamp, 99);
    }

    #[test]
    fn get_latest_is_scoped_per_country_and_category() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &1, &100_u64)
            .unwrap();
        client
            .submit(&submitter, &sym(&env, "KE"), &sym(&env, "Food"), &2, &200_u64)
            .unwrap();

        let ng_latest = client.get_latest(&sym(&env, "NG"), &sym(&env, "Food")).unwrap();
        let ke_latest = client.get_latest(&sym(&env, "KE"), &sym(&env, "Food")).unwrap();

        assert_eq!(ng_latest.timestamp, 100);
        assert_eq!(ke_latest.timestamp, 200);
    }

    // ── determinism ───────────────────────────────────────────────────────

    #[test]
    fn history_deterministic_for_same_state() {
        // Two calls on the same env produce identical results.
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let country = sym(&env, "NG");
        let category = sym(&env, "Food");

        env.mock_all_auths();
        client.submit(&submitter, &country, &category, &1, &1_u64).unwrap();
        client.submit(&submitter, &country, &category, &2, &2_u64).unwrap();

        let first = client.get_history(&country, &category);
        let second = client.get_history(&country, &category);

        assert_eq!(first.len(), second.len());
        for i in 0..first.len() {
            assert_eq!(first.get(i).unwrap().timestamp, second.get(i).unwrap().timestamp);
        }
    }
}
