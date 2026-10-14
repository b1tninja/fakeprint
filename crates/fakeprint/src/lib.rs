//! fakeprint library: profiles, control-plane state, the WSD Print Service, and the embeddable
//! server ([`server::serve`], [`server::router`]).

pub mod daemon;
pub mod discovery;
pub mod ipp;
pub mod jetdirect;
pub mod lab;
pub mod manager;
pub mod manager_http;
#[cfg(windows)]
mod pipe;
pub mod profile;
pub mod registry;
pub mod server;
pub mod state;
#[cfg(all(windows, feature = "tray"))]
pub mod tray;
pub mod tray_model;
#[cfg(feature = "ui")]
pub mod ui;
pub mod wsd;

pub use lab::{is_fake_uuid, FAKE_UUID_PREFIX};
