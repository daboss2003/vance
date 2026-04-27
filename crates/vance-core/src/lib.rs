//! `vance-core` — fundamental types shared across the workspace.
//!
//! Lives at the bottom of the dependency DAG. Depends on no other Vance crate.
//!
//! See PLAN.md §6 for this crate's role and §6.1 for the dep rules.

#![warn(missing_docs)]

pub mod error;
pub mod rational;
pub mod time;

pub use error::{Error, Result};
pub use rational::Rational;
pub use time::{nominal_fps, FrameRate, RationalTime, Timecode};
