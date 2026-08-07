//! lens library crate. The binary (`main.rs`) wires these modules into an
//! MCP stdio server; tests use them directly.

pub mod clean_cli;
pub mod client;
pub mod darkroom;
pub mod disabled;
pub mod discovery;
pub mod index;
pub mod obs;
pub mod qcli;
pub mod routing;
pub mod rtk;
pub mod server;
pub mod session;
pub mod setup;
pub mod status_cli;
pub mod store;
pub mod tools;
pub mod warmup;
pub mod wrap;
