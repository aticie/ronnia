//! Durable state: a single-writer SQLite actor plus the persistence operations
//! for accounts, deliveries, stream presence, statistics, and beatmap caching.
//!
//! Each submodule owns one aggregate. `Store` is the only type callers name, and
//! every method keeps the signature it had before the split.

mod actor;
mod cache;
mod deliveries;
mod schema;
mod sql;
mod statistics;
mod streams;
mod users;

pub use actor::Store;
pub use deliveries::{ClaimOutcome, DeliveryOutcome};
pub(crate) use users::refresh_eligibility;

#[cfg(test)]
mod tests;
