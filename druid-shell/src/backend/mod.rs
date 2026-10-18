// Copyright 2019 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Platform specific implementations.

// It would be clearer to use cfg_if! macros here, but that breaks rustfmt.

#[cfg(feature = "tui")]
mod tui;
#[cfg(feature = "tui")]
pub use tui::*;

#[cfg(all(target_os = "windows", not(feature = "tui")))]
mod windows;
#[cfg(all(target_os = "windows", not(feature = "tui")))]
pub use windows::*;

#[cfg(all(target_os = "macos", not(feature = "tui")))]
mod mac;
#[cfg(all(target_os = "macos", not(feature = "tui")))]
pub use mac::*;
#[cfg(all(target_os = "macos", not(feature = "tui")))]
pub(crate) mod shared;

#[cfg(all(
    feature = "x11",
    not(feature = "tui"),
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
mod x11;
#[cfg(all(
    feature = "x11",
    not(feature = "tui"),
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
pub use x11::*;
#[cfg(all(
    feature = "x11",
    not(feature = "tui"),
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
pub(crate) mod shared;

#[cfg(all(
    feature = "wayland",
    not(feature = "tui"),
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
mod wayland;
#[cfg(all(
    feature = "wayland",
    not(feature = "tui"),
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
pub use wayland::*;
#[cfg(all(
    feature = "wayland",
    not(feature = "tui"),
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
pub(crate) mod shared;

#[cfg(all(
    not(feature = "tui"),
    not(feature = "x11"),
    not(feature = "wayland"),
    feature = "native",
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
mod gtk;
#[cfg(all(
    not(feature = "tui"),
    not(feature = "x11"),
    not(feature = "wayland"),
    feature = "native",
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
pub use self::gtk::*;
#[cfg(all(
    not(feature = "tui"),
    not(feature = "x11"),
    not(feature = "wayland"),
    feature = "native",
    any(target_os = "freebsd", target_os = "linux", target_os = "openbsd")
))]
pub(crate) mod shared;

#[cfg(all(target_arch = "wasm32", not(feature = "tui")))]
mod web;
#[cfg(all(target_arch = "wasm32", not(feature = "tui")))]
pub use web::*;

#[cfg(not(any(feature = "native", feature = "tui")))]
compile_error!("druid-shell requires either the `native` or `tui` feature");
