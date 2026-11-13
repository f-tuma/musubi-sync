//! The read path cases of `read_path.rs` again, as a second provider with a
//! different `ProviderId` and descriptor (plan K5, registry): the engine
//! looks providers up by id and never branches on it.

const SECOND: bool = true;

include!("cases/harness.rs");
include!("cases/read_path.rs");
include!("cases/k5.rs");
