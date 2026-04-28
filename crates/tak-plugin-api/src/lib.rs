//! tak-rs wasm plugin contract — guest-side bindings.
//!
//! This crate is what plugin authors depend on. They `cargo add
//! tak-plugin-api`, then `impl` the [`Guest`] trait exported by the
//! macro-generated [`bindings`] module. The compiled output is a
//! wasm component the host loads at runtime.
//!
//! Host code lives in `tak-plugin-host`; that crate generates its
//! *own* bindings against the same `wit/firehose.wit` file via
//! `wasmtime::component::bindgen!`. The two sides of the boundary
//! agree on the WIT package version
//! (`tak:plugin@0.1.0`) — bumping the package version is how we
//! evolve the contract without breaking deployed plugins
//! silently.
//!
//! See `docs/decisions/0004-wasm-plugins.md` for design intent.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![warn(missing_docs, missing_debug_implementations)]
#![no_std]

extern crate alloc;

#[allow(missing_docs, missing_debug_implementations, clippy::mem_forget)] // macro-generated
pub mod bindings {
    //! Generated guest bindings for the `tak:plugin/firehose` WIT.
    //!
    //! Plugin authors implement [`exports::tak::plugin::inbound::Guest`]
    //! and call `export!(MyPlugin)` to register the impl with the
    //! component-model export table.
    wit_bindgen::generate!({
        path: "wit",
        world: "tak-plugin",
        // pub_export_macro lets plugin authors call `export!(...)`
        // from their own crate without re-importing the macro.
        pub_export_macro: true,
        // We do NOT ship `runtime` here — wit-bindgen's runtime
        // helpers come in via the `wit-bindgen` crate's normal
        // dep tree. Setting this just keeps the macro from
        // assuming a `tak_plugin_api::__runtime` module exists.
    });
}

pub use bindings::export;
pub use bindings::exports::tak::plugin::inbound::{Action, CotEvent, Guest};
/// Convenience re-exports plugin authors will reach for.
pub use bindings::tak::plugin::clock;
pub use bindings::tak::plugin::log;
