//! Public command API behind the `pmux` CLI, split by concern.

mod attach;
mod automation;
mod lifecycle;
mod reply;
mod respawn;
mod setup;

pub use attach::*;
pub use automation::*;
pub use lifecycle::*;
pub use reply::*;
pub use respawn::*;
pub use setup::*;

#[cfg(test)]
pub(crate) mod test_support {
    // Set up a temporary HOME directory for isolated state tests. HOME and
    // SHELL are restored afterwards, even if the test panics.
    pub(crate) fn with_temp_home<F: FnOnce(&std::path::Path)>(f: F) {
        let mut env = crate::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HOME", tmp.path());
        env.set("SHELL", "/bin/bash");
        f(tmp.path());
    }
}
