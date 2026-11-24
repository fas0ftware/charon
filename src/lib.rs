//! Charon: carry files across your own network with one-time, expiring, TLS links.

pub mod cli;
pub mod http;
pub mod server;
pub mod share;
pub mod tls;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
