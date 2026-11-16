//! All cooperating stores share this permanent lock inode. Each operation
//! opens its own handle, including separate operations in the same process.

use super::*;

pub(super) struct StoreLock(std::fs::File);

impl StoreLock {
    pub(super) fn acquire(
        directory: &Dir,
        exclusive: bool,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Self, SessionStoreError> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        if exclusive {
            options.write(true).create(true);
        }
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NONBLOCK);
        }
        let file = match directory.open_with(".store.lock", &options) {
            Err(error) if !exclusive && error.kind() == std::io::ErrorKind::NotFound => {
                options.write(true).create(true);
                directory
                    .open_with(".store.lock", &options)
                    .map_err(store_io)?
            }
            result => result.map_err(store_io)?,
        };
        if !file.metadata().map_err(store_io)?.is_file() {
            return Err(store_io(std::io::Error::other(
                "session lock is not a regular file",
            )));
        }
        let file = file.into_std();
        if let Some(cancel) = cancel {
            loop {
                check_scan_cancelled(cancel)?;
                let result = if exclusive {
                    file.try_lock()
                } else {
                    file.try_lock_shared()
                };
                match result {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(std::fs::TryLockError::Error(error)) => return Err(store_io(error)),
                }
            }
        } else if exclusive {
            file.lock().map_err(store_io)?;
        } else {
            file.lock_shared().map_err(store_io)?;
        }
        Ok(Self(file))
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Also release a description temporarily inherited by a concurrent fork.
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn reading_an_existing_archive_does_not_require_writable_lock_access() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
        store.save(&mut record).unwrap();
        std::fs::set_permissions(
            store.root.join(".store.lock"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        assert_eq!(store.load(&record.id).unwrap(), record);
    }

    #[test]
    fn independent_read_handles_keep_writers_excluded_until_the_last_reader_releases() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let first = StoreLock::acquire(&store.directory, false, None).unwrap();
        let second = StoreLock::acquire(&store.directory, false, None).unwrap();
        let writer = store
            .directory
            .open_with(".store.lock", OpenOptions::new().read(true).write(true))
            .unwrap()
            .into_std();
        assert!(matches!(
            writer.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        assert!(matches!(
            writer.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(second);
        writer.try_lock().unwrap();
        writer.unlock().unwrap();
    }

    #[test]
    fn cancellation_interrupts_a_scan_waiting_for_another_writer() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
        store.save(&mut record).unwrap();
        let writer = StoreLock::acquire(&store.directory, true, None).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(store.list_cancellable(&worker_cancel))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            result_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        cancel.cancel();
        assert!(matches!(
            result_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            Err(SessionStoreError::Cancelled)
        ));
        // Cancellation returned even though the exclusive writer is still held.
        drop(writer);
        worker.join().unwrap();
    }

    #[test]
    fn cancellation_interrupts_a_load_without_waiting_for_the_writer_to_release() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
        record
            .entries
            .push(Arc::new(crate::SessionEntry::user("resume this session")));
        store.save(&mut record).unwrap();
        let writer = StoreLock::acquire(&store.directory, true, None).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker_store = store.clone();
        let id = record.id.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(worker_store.load_cancellable(&id, &worker_cancel))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            result_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        cancel.cancel();
        assert!(matches!(
            result_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            Err(SessionStoreError::Cancelled)
        ));
        worker.join().unwrap();
        // A request cancelled before loading must also return while the same
        // writer still holds the directory lock.
        assert!(matches!(
            store.load_cancellable(&record.id, &cancel),
            Err(SessionStoreError::Cancelled)
        ));
        drop(writer);
        assert_eq!(
            store
                .load_cancellable(&record.id, &tokio_util::sync::CancellationToken::new())
                .unwrap(),
            record
        );
    }
}
