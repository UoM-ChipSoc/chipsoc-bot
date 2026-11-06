//! The client side of admin work, for the CLI and TUI: the web server's JSON
//! API ([`remote`]: its wire types, a client and the saved sign-in), and
//! [`backend::Backend`], which does the same work either through it or here
//! on the config files. No web server in here, so the TUI and CLI don't
//! carry one.

pub mod backend;
pub mod remote;

pub use backend::{Backend, Local};
pub use remote::{Client, Login, Planned, Step};
