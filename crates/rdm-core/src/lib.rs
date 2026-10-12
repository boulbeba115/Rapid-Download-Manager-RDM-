//! Rapid Download Manager — download engine.
//!
//! No GUI code lives here. See `SPEC.md` §5 for the design this implements.

pub mod api;
pub mod category;
pub mod error;
pub mod filename;
pub mod http;
pub mod limiter;
pub mod manager;
pub mod segment;
pub mod state;
pub mod task;
pub mod util;
pub mod writer;

#[cfg(feature = "testserver")]
pub mod testserver;

pub use error::{Error, Result};
pub use http::{NetConfig, Probe, ProxyConfig, RemoteInfo, RequestSpec};
pub use limiter::RateLimiter;
pub use manager::{Config, Download, Event, Id, Manager, NewDownload, Queue, Status, View};
pub use task::{Connections, Outcome, Progress, Start, TaskConfig, TaskHandle};
