//! Configuration for the Relay CLI and server.
#![warn(missing_docs)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/getsentry/relay/master/artwork/relay-icon.png",
    html_favicon_url = "https://raw.githubusercontent.com/getsentry/relay/master/artwork/relay-icon.png"
)]
#![allow(clippy::derive_partial_eq_without_eq)]

pub mod aggregator;
mod byte_size;
mod config;
mod redis;
mod upstream;

pub use crate::aggregator::{AggregatorServiceConfig, ScopedAggregatorConfig};
pub use crate::byte_size::*;
pub use crate::config::*;
pub use crate::redis::*;
pub use crate::upstream::*;
