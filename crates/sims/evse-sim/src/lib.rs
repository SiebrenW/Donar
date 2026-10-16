//! A simulated EVSE: the mirror image of the EV session machine, built from the same codecs.
//! It is sans-IO like the EV side ([`Evse::handle`]), with a small blocking TCP server
//! ([`serve_one`]) for end-to-end tests. Its knobs inject the faults an EVSE test platform
//! needs: rejections, failing response codes and silence.

pub mod slac;

mod config;
mod evse;
mod live;
mod net;

pub use config::{Config, Limits, OfferedSet, Settings, TlsVersions, value_name};
pub use live::EvseLive;
pub use evse::{Evse, Output, State};
pub use net::{answer_discovery, serve, serve_live, serve_one};
