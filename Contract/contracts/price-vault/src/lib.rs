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

        env.storage().persistent().set(
            &DataKey::Price(country_iso.clone(), category.clone(), timestamp),
            &PriceSubmission {
                submitter,
                country_iso,
                category,
                value,
                timestamp,
                status: SubmissionStatus::Pending, // always starts Pending
            },
        );
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
        assert!(result.is_none());
    }

    #[test]
    fn get_status_returns_current_state_after_each_transition() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);
        let verifier = Address::generate(&env);
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
    }
}
