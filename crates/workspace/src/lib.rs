pub mod paths;
pub use paths::{IdentityKind, WorkspacePaths, absolutise};
pub mod prompt;
pub mod walk;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(feature = "io")]
pub mod identity;
#[cfg(feature = "io")]
pub use identity::{IdentityFiles, load_identity_files, write_identity_file};

#[cfg(feature = "io")]
pub mod singleton;
#[cfg(feature = "io")]
pub use singleton::{WorkspaceLock, acquire_workspace_lock};

#[cfg(feature = "io")]
mod manager;
#[cfg(feature = "io")]
pub use manager::WorkspaceManager;
