//! Scenarios: a simulated vehicle and a charger (or an external implementation) run against
//! each other in the sandbox, described by TOML files ([`scenario`]) and executed by the
//! runner ([`runner`], Linux and root only). The integration tests of this crate are the
//! end-to-end tests: wrappers that run a scenario file and assert that it passed.

pub mod pki;
pub mod scenario;

#[cfg(target_os = "linux")]
pub mod partners;
#[cfg(target_os = "linux")]
pub mod runner;

pub use scenario::Scenario;
