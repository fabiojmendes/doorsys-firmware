#![no_std]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

extern crate alloc;

pub mod access;
pub mod clock;
pub mod commands;
pub mod config;
pub mod door;
pub mod health;
pub mod mqtt;
pub mod shell;
pub mod storage;
pub mod user_db;
pub mod wiegand;
pub mod wifi;

pub mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

/// Allocate a `'static` mutable reference backed by a [`static_cell::StaticCell`].
#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}
