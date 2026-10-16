//! Scenarios: simulated vehicle and charger, or external partner implementations, run in the
//! sandbox. (The integration tests of this crate are the end-to-end tests; scenario files and
//! the runner follow.)
#![cfg(target_os = "linux")]

pub mod partners;
