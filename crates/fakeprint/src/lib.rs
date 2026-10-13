//! fakeprint library: profiles, control-plane state, the WSD Print Service, and the embeddable
//! server ([`server::serve`], [`server::router`]).

pub mod discovery;
pub mod ipp;
pub mod jetdirect;
pub mod lab;
#[cfg(windows)]
mod pipe;
pub mod profile;
pub mod server;
pub mod state;
pub mod wsd;
