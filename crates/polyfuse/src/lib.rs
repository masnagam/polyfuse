//! A FUSE (Filesystem in Userspace) library for Rust.

#![doc(html_root_url = "https://docs.rs/polyfuse/0.4.0")]
#![forbid(clippy::todo, clippy::unimplemented)]

mod conn;
mod decoder;
mod session;

pub mod atomic_bytes;
pub mod op;
pub mod reply;

pub use crate::{
    op::Operation,
    session::{KernelConfig, Notifier, Request, Session},
};
