use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// Single-instance lock. The bound socket doubles as the control socket (see `core::control`).
pub struct InstanceLock {
    path: PathBuf,
    listener: UnixListener,
}

impl InstanceLock {
    pub fn acquire() -> Result<Self> {
        acquire_from_path(default_socket_path())
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_from_path(path: PathBuf) -> Result<InstanceLock> {
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!(
                "another slycapture instance is already running (lock: {})",
                path.display()
            );
        }

        fs::remove_file(&path).with_context(|| {
            format!(
                "failed to remove stale slycapture instance lock file: {}",
                path.display()
            )
        })?;
    }

    let listener = UnixListener::bind(&path).with_context(|| {
        format!(
            "failed to create slycapture instance lock socket: {}",
            path.display()
        )
    })?;

    Ok(InstanceLock { path, listener })
}

pub fn default_socket_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime_dir.join(format!("slycapture-{}.sock", user_hint()))
}

fn user_hint() -> String {
    std::env::var("USER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "user".to_string())
}

#[cfg(test)]
mod tests {
    use super::acquire_from_path;
    use std::path::PathBuf;

    fn test_lock_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "slycapture-test-lock-{}-{}.sock",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn rejects_second_lock_holder() {
        let path = test_lock_path("second-holder");
        let first = acquire_from_path(path.clone()).expect("first lock should succeed");
        let second = acquire_from_path(path.clone());

        assert!(second.is_err(), "second lock should fail");

        drop(first);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovers_from_stale_socket_file() {
        let path = test_lock_path("stale-socket");
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("create stale listener");
        drop(stale);

        let lock = acquire_from_path(path.clone()).expect("lock should recover from stale path");
        drop(lock);

        let _ = std::fs::remove_file(path);
    }
}
