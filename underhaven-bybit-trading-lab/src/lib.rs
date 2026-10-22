//! # UNDERHAVEN BYBIT TRADING LAB
//!
//! A paper-only research platform for Bybit USDT perpetual futures.
//!
//! ## Paper-only boundary
//!
//! This crate deliberately contains no code path that can submit an order to
//! Bybit. Market data is public; optional API credentials are encrypted, marked
//! read-only and used only for account/fee metadata. The execution engine writes
//! exclusively to the local SQLite paper ledger. See `docs/SAFETY.md` and the
//! `paper_only_boundary` test module.
//!
//! ## Module map
//!
//! * [`config`] – environment configuration and validated defaults.
//! * [`model`] – canonical domain types.
//! * [`indicators`] – pure indicator primitives.
//! * [`strategy`] – the three independent strategy evaluators.
//! * [`costs`] – cost accounting and the edge-validation gate.
//! * [`risk`] – sizing and hard risk gates.
//! * [`metrics`] – shared performance statistics.
//! * [`backtest`] – historical simulation, segments, sensitivity, walk-forward.
//! * [`auth`] – Argon2id sessions, CSRF and encrypted read-only credentials.
//! * [`app`] – runtime orchestration and dashboard aggregation.
//! * [`web`] – authenticated Axum dashboard, SSE stream and JSON API.

pub const APP_NAME: &str = "UNDERHAVEN BYBIT TRADING LAB";
pub const PAPER_ONLY: bool = true;

pub mod app;
pub mod auth;
pub mod backtest;
pub mod bybit;
pub mod config;
pub mod costs;
pub mod indicators;
pub mod market;
pub mod metrics;
pub mod model;
pub mod paper;
pub mod risk;
pub mod store;
pub mod strategy;
pub mod web;
pub mod ws;

/// Compile-time assertions that the paper-only policy cannot be flipped by a
/// runtime environment variable or a feature flag.
#[cfg(test)]
mod paper_only_boundary {
    use super::*;

    // Compile-time enforcement: flipping either constant breaks the build.
    const _: () = assert!(PAPER_ONLY);
    const _: () = assert!(model::PAPER_ONLY);

    #[test]
    fn compiled_as_paper_only() {
        assert_eq!(super::config::BUILD_VERSION, "underhaven/0.1.0");
        assert_eq!(model::INITIAL_EQUITY, 100.0);
    }
}
