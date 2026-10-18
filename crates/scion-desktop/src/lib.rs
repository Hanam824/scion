//! Scion Desktop logic, kept free of UI types so it can be tested headless.
//! `main.rs` holds the Slint window and maps `worker::Event`s onto it.

pub mod config;
pub mod highlight;
pub mod integrations;
pub mod local_server;
pub mod model;
pub mod watch;
pub mod worker;
