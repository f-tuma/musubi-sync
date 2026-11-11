//! Google Calendar adapter: `Provider`, `PushSource` and `Codec<Event>`.
//!
//! The adapter knows neither the engine nor the storage. Only `musubi-syncd`
//! and tests may depend on this crate.

mod codec;

pub use codec::GoogleCodec;
mod provider;
mod push;

pub use provider::{GoogleConfig, GoogleProvider};
