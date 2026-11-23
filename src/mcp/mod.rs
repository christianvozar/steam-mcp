//! Model Context Protocol server implementation (revision 2026-07-28).
//!
//! * [`server::Core`] — transport-independent dispatcher.
//! * [`tools`], [`resources`], [`prompts`], [`completion`] — server features.
//! * [`subscriptions`] — `subscriptions/listen` streams.
//! * [`tasks`] — the `io.modelcontextprotocol/tasks` extension.
//! * [`state`] — integrity-protected MRTR `requestState`.

pub mod agents;
pub mod completion;
pub mod context;
pub mod prompts;
pub mod protocol;
pub mod resources;
pub mod schema;
pub mod server;
pub mod state;
pub mod subscriptions;
pub mod tasks;
pub mod tools;
