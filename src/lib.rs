//! This crate provides a pure-rust library for reading ZIM files.
//!
//! ZIM files are a format used primarily to store wikis (such as Wikipedia and others based on
//! MediaWiki).
//!
//! For more into, see the [OpenZIM website](http://www.openzim.org/wiki/OpenZIM)
//!

mod cluster;
mod directory_entry;
mod directory_iterator;
mod errors;
mod mime_type;
mod namespace;
mod store;
mod target;
mod uuid;
mod zim;

pub use crate::cluster::{Cluster, ClusterGuard};
pub use crate::directory_entry::DirectoryEntry;
pub use crate::errors::{Error, Result};
pub use crate::mime_type::MimeType;
pub use crate::namespace::Namespace;
pub use crate::store::Store;
pub use crate::target::Target;
pub use crate::uuid::Uuid;
pub use crate::zim::{Content, Listing, Zim};
