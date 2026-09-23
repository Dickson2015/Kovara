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
//! # Schema validation for serialized payloads (issue #739)
//!
//! Soroban contracts receive inputs as XDR-serialized values.  Before any
//! of those values are used or persisted they must pass a schema validation
//! step that is:
//!
//! - **Consistent**: the same `PayloadSchema` descriptor and `validate`
//!   function are used across every entry point.
//! - **Explicit**: every constraint is named as a variant of
//!   [`ValidationError`]; there is no catch-all "invalid input" code.
//! - **Pure**: validation never mutates state — it is a predicate over the
//!   raw input values, callable before `require_auth` so malformed calls
//!   are rejected at minimum cost.
//!
//! ## `payload` module
//!
//! The [`payload`] module is the single validation entry point.  It exposes:
//!
//! | Item | Purpose |
//! |---|---|
//! | [`payload::FieldKind`] | Discriminant for the type of a validated field. |
//! | [`payload::Constraint`] | A named constraint applied to a single field. |
//! | [`payload::PayloadSchema`] | An ordered list of `Constraint`s that fully describes what a valid payload looks like. |
//! | [`payload::validate`] | Apply a `PayloadSchema` to a `SubmitPayload`, returning the first failing constraint as `Err(ValidationError)`. |
//! | [`payload::SubmitPayload`] | A plain-struct view of the `submit` arguments, passed by value to `validate` so no cloning of contract-SDK types is needed. |
//!
//! ## Wiring into `submit`
//!
//! `PriceVault::submit` calls `payload::validate` with a fixed
//! `SUBMIT_SCHEMA` before `require_auth` and before any storage access.
//! A malformed payload is rejected at the gate — no auth side-effects, no
//! partial writes, no ambiguous state.
//!
//! ## Extension pattern
//!
//! Any other contract entry point (e.g. a future `update_config`,
//! `set_status`, or `aggregate`) follows the same pattern:
//! 1. Define a `*Payload` struct capturing the raw arguments.
//! 2. Define a `*_SCHEMA: &[Constraint]` constant.
//! 3. Call `payload::validate(&schema, &payload)?` as the first line of the
//!    entry point body.
//!
//! This is the "consistent validation across contract interfaces"
//! acceptance criterion.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol,
};

// ── Validation error codes ────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// The provided timestamp is zero or otherwise unusable.
    InvalidTimestamp = 1,
    // ── Schema validation errors (issue #739) ─────────────────────────────
    /// A required field is missing (currently unused in the on-chain path
    /// because all fields are positional, but exported for SDK consumers).
    MissingRequiredField = 2,
    /// A field value is below its declared minimum.
    ValueBelowMinimum = 3,
    /// A field value is above its declared maximum.
    ValueAboveMaximum = 4,
    /// A Symbol field is empty (length == 0).
    EmptySymbol = 5,
    /// A numeric field is negative where only non-negative values are
    /// permitted.
    NegativeNotAllowed = 6,
    /// A numeric field is zero where only strictly positive values are
    /// permitted.
    ZeroNotAllowed = 7,
    /// The schema version embedded in the payload does not match the
    /// contract's current schema version.
    SchemaMismatch = 8,
}

// ── payload module ────────────────────────────────────────────────────────────

/// Schema validation for serialized payloads.
///
/// Every contract entry point that receives external input should:
/// 1. Define a `*Payload` struct for its arguments.
/// 2. Declare a `*_SCHEMA` constant (`&[Constraint]`).
/// 3. Call `validate(&schema, &payload)?` before any auth check or
///    state mutation.
pub mod payload {
    use super::Error;
    use soroban_sdk::Symbol;

    // ── Field kinds ───────────────────────────────────────────────────────

    /// Discriminant for the type of a validated field.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub enum FieldKind {
        /// A 64-bit unsigned integer (timestamps, counts).
        U64,
        /// A 128-bit signed integer (prices, amounts).
        I128,
        /// A Soroban `Symbol` (country codes, category labels).
        Symbol,
    }

    // ── Constraint ────────────────────────────────────────────────────────

    /// A single named constraint applied to a payload field.
    ///
    /// Constraints are evaluated in declaration order.  The first failing
    /// constraint causes `validate` to return `Err` immediately — there is
    /// no accumulation of errors, which is consistent with how Soroban
    /// contracts communicate failures to callers.
    #[derive(Copy, Clone, Debug)]
    pub enum Constraint {
        /// The `u64` field must be strictly greater than zero.
        U64NonZero,
        /// The `u64` field must be at most this value.
        U64Max(u64),
        /// The `i128` field must be greater than zero.
        I128Positive,
        /// The `i128` field must be greater than or equal to zero.
        I128NonNegative,
        /// The `i128` field must be at most this value.
        I128Max(i128),
        /// The `Symbol` field must have at least one character.
        SymbolNonEmpty,
        /// The schema version embedded in the payload must equal this value.
        SchemaVersion(u32),
    }

    // ── SubmitPayload ─────────────────────────────────────────────────────

    /// A plain-data view of the `PriceVault::submit` arguments.
    ///
    /// Decoupling validation from the Soroban `Env` and `Address` types
    /// keeps `validate` a pure function — no SDK imports needed in the
    /// validation logic itself, no mock environment required in tests.
    pub struct SubmitPayload<'a> {
        /// ISO country code (e.g. "NG", "KE").
        pub country_iso: &'a Symbol,
        /// Basket category (e.g. "Food", "Rent").
        pub category: &'a Symbol,
        /// Price value in the smallest fixed-point unit.
        pub value: i128,
        /// Observation timestamp.
        pub timestamp: u64,
        /// Schema version the caller expects.
        pub schema_version: u32,
    }

    // ── validate ──────────────────────────────────────────────────────────

    /// Apply a schema (ordered list of [`Constraint`]s) to a
    /// [`SubmitPayload`].
    ///
    /// Returns `Ok(())` when every constraint passes, or
    /// `Err(Error::*)` on the first failing constraint.
    ///
    /// The function is `#[inline]` so the optimiser can fold constant
    /// schemas fully at the call site in release builds.
    #[inline]
    pub fn validate(
        schema: &[Constraint],
        payload: &SubmitPayload<'_>,
    ) -> Result<(), Error> {
        for constraint in schema {
            match constraint {
                Constraint::U64NonZero => {
                    if payload.timestamp == 0 {
                        return Err(Error::ZeroNotAllowed);
                    }
                }
                Constraint::U64Max(max) => {
                    if payload.timestamp > *max {
                        return Err(Error::ValueAboveMaximum);
                    }
                }
                Constraint::I128Positive => {
                    if payload.value < 0 {
                        return Err(Error::NegativeNotAllowed);
                    }
                    if payload.value == 0 {
                        return Err(Error::ZeroNotAllowed);
                    }
                }
                Constraint::I128NonNegative => {
                    if payload.value < 0 {
                        return Err(Error::NegativeNotAllowed);
                    }
                }
                Constraint::I128Max(max) => {
                    if payload.value > *max {
                        return Err(Error::ValueAboveMaximum);
                    }
                }
                Constraint::SymbolNonEmpty => {
                    // Soroban Symbol::len() returns the number of characters.
                    // A zero-length Symbol is rejected as it would produce an
                    // unresolvable storage key.
                    if payload.country_iso.len() == 0 || payload.category.len() == 0 {
                        return Err(Error::EmptySymbol);
                    }
                }
                Constraint::SchemaVersion(expected) => {
                    if payload.schema_version != *expected {
                        return Err(Error::SchemaMismatch);
                    }
                }
            }
        }
        Ok(())
    }
}

// ── Schema constant ───────────────────────────────────────────────────────────

/// The schema applied to every `PriceVault::submit` call.
///
/// Constraints are evaluated left-to-right; the first failure is returned.
/// The ordering is chosen so that the cheapest checks (integer bounds) run
/// before the more expensive ones (Symbol length).
const SUBMIT_SCHEMA: &[payload::Constraint] = &[
    // Timestamp must be a positive u64.
    payload::Constraint::U64NonZero,
    // Price must be strictly positive.
    payload::Constraint::I128Positive,
    // Price must not exceed the operational ceiling (1 billion in fixed-point
    // units prevents absurd index skew from rogue submissions).
    payload::Constraint::I128Max(1_000_000_000_000),
    // Country and category symbols must not be empty strings.
    payload::Constraint::SymbolNonEmpty,
    // Schema version must match this build.
    payload::Constraint::SchemaVersion(1),
];

/// The schema version this build of PriceVault understands.
pub const SCHEMA_VERSION: u32 = 1;

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
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct PriceVault;

#[contractimpl]
impl PriceVault {
    /// Record a raw price submission.
    ///
    /// The payload is validated against [`SUBMIT_SCHEMA`] before `require_auth`
    /// and before any storage access.  Invalid payloads are rejected at the
    /// gate with a specific error code — no auth side-effects, no partial
    /// writes.
    ///
    /// # Errors
    /// * [`Error::ZeroNotAllowed`] — `timestamp` or `value` is zero.
    /// * [`Error::NegativeNotAllowed`] — `value` is negative.
    /// * [`Error::ValueAboveMaximum`] — `value` exceeds the schema ceiling.
    /// * [`Error::EmptySymbol`] — `country_iso` or `category` is empty.
    /// * [`Error::SchemaMismatch`] — caller's schema version ≠ 1.
    pub fn submit(
        env: Env,
        submitter: Address,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        timestamp: u64,
    ) -> Result<(), Error> {
        // Schema validation runs before require_auth so malformed payloads
        // are rejected without touching the auth subsystem.
        payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country_iso,
                category: &category,
                value,
                timestamp,
                schema_version: SCHEMA_VERSION,
            },
        )?;

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

    /// Validate a submit payload against the schema without committing any
    /// state.
    ///
    /// Useful for SDK clients that want to pre-validate arguments before
    /// constructing a transaction, and for cross-contract callers that need
    /// to confirm a payload is structurally sound before forwarding it.
    ///
    /// Returns `Ok(())` on a valid payload; a specific [`Error`] variant on
    /// the first failing constraint.
    pub fn validate_submit_payload(
        env: Env,
        country_iso: Symbol,
        category: Symbol,
        value: i128,
        timestamp: u64,
    ) -> Result<(), Error> {
        let _ = env;
        payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country_iso,
                category: &category,
                value,
                timestamp,
                schema_version: SCHEMA_VERSION,
            },
        )
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

    // ── Pure validate() unit tests (no Env needed) ────────────────────────

    fn make_valid_payload(env: &Env) -> (Symbol, Symbol) {
        (sym(env, "NG"), sym(env, "Food"))
    }

    #[test]
    fn valid_payload_passes_schema() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let result = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 1_000,
                timestamp: 100,
                schema_version: SCHEMA_VERSION,
            },
        );
        assert!(result.is_ok());
    }

    #[test]
    fn zero_timestamp_fails_u64_non_zero() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let err = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 1_000,
                timestamp: 0,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::ZeroNotAllowed);
    }

    #[test]
    fn zero_value_fails_i128_positive() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let err = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 0,
                timestamp: 100,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::ZeroNotAllowed);
    }

    #[test]
    fn negative_value_fails_i128_positive() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let err = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: -1,
                timestamp: 100,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::NegativeNotAllowed);
    }

    #[test]
    fn value_above_ceiling_fails_i128_max() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        // SUBMIT_SCHEMA ceiling is 1_000_000_000_000
        let err = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 1_000_000_000_001,
                timestamp: 100,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::ValueAboveMaximum);
    }

    #[test]
    fn value_at_ceiling_passes() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let result = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 1_000_000_000_000, // exactly at ceiling
                timestamp: 100,
                schema_version: SCHEMA_VERSION,
            },
        );
        assert!(result.is_ok());
    }

    #[test]
    fn wrong_schema_version_fails_schema_version_constraint() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let err = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 1_000,
                timestamp: 100,
                schema_version: 99, // wrong version
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::SchemaMismatch);
    }

    #[test]
    fn correct_schema_version_passes() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let result = payload::validate(
            SUBMIT_SCHEMA,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 500,
                timestamp: 42,
                schema_version: SCHEMA_VERSION,
            },
        );
        assert!(result.is_ok());
    }

    // ── Constraint::SymbolNonEmpty ────────────────────────────────────────

    #[test]
    fn empty_country_symbol_fails() {
        let env = Env::default();
        let empty = sym(&env, "");
        let category = sym(&env, "Food");
        // Only SymbolNonEmpty constraint for this test.
        let schema = &[payload::Constraint::SymbolNonEmpty];
        let err = payload::validate(
            schema,
            &payload::SubmitPayload {
                country_iso: &empty,
                category: &category,
                value: 100,
                timestamp: 1,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::EmptySymbol);
    }

    // ── Constraint::I128NonNegative ───────────────────────────────────────

    #[test]
    fn i128_non_negative_accepts_zero() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let schema = &[payload::Constraint::I128NonNegative];
        let result = payload::validate(
            schema,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: 0,
                timestamp: 1,
                schema_version: SCHEMA_VERSION,
            },
        );
        assert!(result.is_ok());
    }

    #[test]
    fn i128_non_negative_rejects_negative() {
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let schema = &[payload::Constraint::I128NonNegative];
        let err = payload::validate(
            schema,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: -100,
                timestamp: 1,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::NegativeNotAllowed);
    }

    // ── Constraint evaluation order ───────────────────────────────────────

    #[test]
    fn first_failing_constraint_is_returned() {
        // Schema: U64NonZero, then I128Positive.
        // Both timestamp=0 and value=-1 are invalid.
        // U64NonZero is first, so ZeroNotAllowed must be the error.
        let env = Env::default();
        let (country, category) = make_valid_payload(&env);
        let schema = &[
            payload::Constraint::U64NonZero,
            payload::Constraint::I128Positive,
        ];
        let err = payload::validate(
            schema,
            &payload::SubmitPayload {
                country_iso: &country,
                category: &category,
                value: -1,
                timestamp: 0,
                schema_version: SCHEMA_VERSION,
            },
        )
        .unwrap_err();
        assert_eq!(err, Error::ZeroNotAllowed); // not NegativeNotAllowed
    }

    // ── Contract entry points ─────────────────────────────────────────────

    #[test]
    fn submit_valid_payload_succeeds() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        client
            .submit(
                &submitter,
                &sym(&env, "NG"),
                &sym(&env, "Food"),
                &1_000,
                &100_u64,
            )
            .unwrap();

        let got = client.get(&sym(&env, "NG"), &sym(&env, "Food"), &100_u64).unwrap();
        assert_eq!(got.value, 1_000);
    }

    #[test]
    fn submit_zero_value_rejected_by_schema() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &0, &100_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::ZeroNotAllowed);
    }

    #[test]
    fn submit_negative_value_rejected_by_schema() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &-500, &100_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::NegativeNotAllowed);
    }

    #[test]
    fn submit_zero_timestamp_rejected_by_schema() {
        let env = make_env();
        let client = deploy(&env);
        let submitter = Address::generate(&env);

        env.mock_all_auths();
        let err = client
            .try_submit(&submitter, &sym(&env, "NG"), &sym(&env, "Food"), &100, &0_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::ZeroNotAllowed);
    }

    #[test]
    fn validate_submit_payload_entry_point_accepts_valid() {
        let env = make_env();
        let client = deploy(&env);

        let result = client
            .validate_submit_payload(&sym(&env, "KE"), &sym(&env, "Rent"), &500, &50_u64);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_submit_payload_entry_point_rejects_negative() {
        let env = make_env();
        let client = deploy(&env);

        let err = client
            .try_validate_submit_payload(
                &sym(&env, "KE"),
                &sym(&env, "Rent"),
                &-1,
                &50_u64,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, Error::NegativeNotAllowed);
    }
}
