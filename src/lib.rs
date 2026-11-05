//! Custom HTTP gateway gateway addon: Admin-defined HTTP gateway.
//! The whole gateway lives in [`custom`]; this file only exports it to the host.
pub mod custom;

#[cfg(all(target_family = "wasm", feature = "export"))]
pelty_addon_sdk::gateway_addon!(custom::descriptor, custom::factory);
