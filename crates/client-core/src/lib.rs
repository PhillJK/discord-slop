//! UI-agnostic qchat client.
//!
//! * [`Session`]: async API over one relay connection (used by bots and tests).
//! * [`runtime`]: background thread + shared [`state::AppState`] for GUI clients.

mod media_glue;
pub mod runtime;
pub mod session;
pub mod state;
pub mod tofu;

pub use qchat_proto as proto;
pub use runtime::{ClientHandle, Command, Waker};
pub use session::{ConnectError, ConnectParams, RelayAddress, RequestError, Session, SessionEvent};
pub use tofu::KnownHosts;
