pub mod paths;
pub use paths::{IdentityKind, WorkspacePaths, absolutise, absolutise_target};
pub mod name;
pub use name::{display_name, rejected_name_removal, rejected_rename, with_display_name};
pub mod prompt;
pub mod walk;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(feature = "io")]
pub mod identity;
#[cfg(feature = "io")]
pub use identity::{
    IdentityFiles, IdentitySource, commit_personas, ensure_named_persona_layout,
    ensure_persona_layout, load_identity, load_identity_files, personas_git_lock,
};

#[cfg(feature = "io")]
pub mod layout;
#[cfg(feature = "io")]
pub use layout::{ensure_layout, seed_default_identity_files};

#[cfg(feature = "io")]
pub mod memory;
#[cfg(feature = "io")]
pub use memory::load_memory_index;

#[cfg(feature = "io")]
pub mod singleton;
#[cfg(feature = "io")]
pub use singleton::{WorkspaceLock, acquire_workspace_lock};
