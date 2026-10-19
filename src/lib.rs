//! Telltale: gauges that stay dark until something needs your attention.
//! Words are defined in GLOSSARY.md, behaviour in SPEC.md.

pub mod format;
pub mod gauge;
pub mod reading;
mod run;

pub use run::{Extra, Given, run, run_with};
