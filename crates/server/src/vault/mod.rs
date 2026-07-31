//! Everything that touches a user's notes directory.
//!
//! [`path`] is the security boundary: it is the only module that turns a
//! client-supplied string into an absolute path. [`store`] performs the actual
//! reads and writes. [`index`] mirrors what it finds into Postgres, and
//! [`watch`] keeps that mirror honest when files change behind the app's back.

pub mod index;
pub mod path;
pub mod store;
pub mod watch;

pub use path::{Vault, VaultPath};
