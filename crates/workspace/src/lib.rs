pub mod paths;
pub use paths::{IdentityKind, WorkspacePaths, absolutise};
pub mod name;
pub use name::{display_name, with_display_name};
pub mod prompt;
pub mod walk;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(feature = "io")]
pub mod identity;
#[cfg(feature = "io")]
pub use identity::{
    IdentityFiles, IdentitySource, ensure_persona_layout, load_identity, load_identity_files,
};

#[cfg(feature = "io")]
pub mod singleton;
#[cfg(feature = "io")]
pub use singleton::{WorkspaceLock, acquire_workspace_lock};

#[cfg(feature = "io")]
mod manager;
#[cfg(feature = "io")]
pub use manager::WorkspaceManager;
