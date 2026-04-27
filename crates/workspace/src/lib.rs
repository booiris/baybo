pub mod paths;
pub use paths::{IdentityKind, WorkspacePaths};

#[cfg(feature = "io")]
pub mod identity;
#[cfg(feature = "io")]
pub use identity::{IdentityFiles, load_identity_files, write_identity_file};

#[cfg(feature = "io")]
mod manager;
#[cfg(feature = "io")]
pub use manager::WorkspaceManager;
