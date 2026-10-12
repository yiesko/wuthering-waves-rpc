//! Unit tests for wwrpc, one module per area (implementation files stay
//! focused on implementation). They run with `cargo test -p wwrpc --lib`.

mod character;
mod db;
mod detect;
mod ipc;
mod log;
mod presence;
mod runtime_dir;
