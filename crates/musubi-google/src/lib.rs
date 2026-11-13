//! Google adapter: Calendar, Tasks and People. `Provider`, `PushSource`,
//! `Codec<Event>`, `Codec<Task>` and `Codec<Contact>`.
//!
//! The adapter knows neither the engine nor the storage. Only `musubi-syncd`
//! and tests may depend on this crate.

mod codec;
mod people;
mod provider;
mod push;
mod tasks;

pub use codec::GoogleCodec;
pub use people::PeopleCodec;
pub use provider::{GoogleConfig, GoogleProvider};
pub use tasks::TasksCodec;
