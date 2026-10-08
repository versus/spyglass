//! Private on-disk locations (owner-only permissions).

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

/// Create `dir` (and parents) and force mode 0700 on it.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}
