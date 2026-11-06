//! Checkout workers own their repository lock until every child has exited.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

fn cache_dir() -> PathBuf {
    cache_dir_under(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

pub(crate) fn cache_dir_under(
    home: Option<std::ffi::OsString>,
    profile: Option<std::ffi::OsString>,
) -> PathBuf {
    [home, profile]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache")
        .join("deep-code")
        .join("swebench-repos")
}

fn repo_lock_for(cache: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    LOCKS
        .lock()
        .expect("repo-lock map poisoned")
        .entry(cache.to_path_buf())
        .or_default()
        .clone()
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct CheckoutControl {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl CheckoutControl {
    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "checkout cancelled"
        );
        anyhow::ensure!(Instant::now() < self.deadline, "checkout timed out");
        Ok(())
    }
}

/// The blocking worker keeps ownership even if its async caller is dropped.
/// Cancellation requests teardown; releasing the repo lock follows kill/wait.
async fn run_worker<T: Send + 'static>(
    cache: PathBuf,
    budget: Duration,
    work: impl FnOnce(CheckoutControl) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let deadline = Instant::now() + budget;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    let lock = tokio::time::timeout(budget, repo_lock_for(&cache).lock_owned())
        .await
        .map_err(|_| anyhow::anyhow!("checkout timed out waiting for repository lock"))?;
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        work(CheckoutControl {
            cancelled,
            deadline,
        })
    })
    .await?
}

/// Check out a benchmark commit through its shared bare cache. The deadline
/// covers lock acquisition, cloning, fetching and checkout. Expiry requests
/// teardown; returning still waits for the process to be reaped.
pub(crate) async fn checkout_repo(
    repo: &str,
    commit: &str,
    dest: &Path,
    budget: Duration,
) -> anyhow::Result<()> {
    let cache = cache_dir().join(repo.replace('/', "__"));
    let source = format!("https://github.com/{repo}.git");
    let commit = commit.to_owned();
    let dest = dest.to_path_buf();
    run_worker(cache.clone(), budget, move |control| {
        checkout_at(&source, &commit, &dest, &cache, &control)
    })
    .await
}

fn checkout_at(
    source: &str,
    commit: &str,
    dest: &Path,
    cache: &Path,
    control: &CheckoutControl,
) -> anyhow::Result<()> {
    let cache_str = cache.to_string_lossy();
    let dest_str = dest.to_string_lossy();
    control.check()?;
    ensure_bare_cache(cache, |staging| {
        git_until(
            &["clone", "--bare", source, &staging.to_string_lossy()],
            control,
        )?;
        control.check()
    })?;
    git_until(
        &["clone", "--shared", "--no-checkout", &cache_str, &dest_str],
        control,
    )?;
    if git_until(&["-C", &dest_str, "checkout", "-q", commit], control).is_err() {
        // A cancellation must not launch a fetch while unwinding.
        control.check()?;
        git_until(
            &[
                "-C",
                &cache_str,
                "fetch",
                "origin",
                "+refs/heads/*:refs/heads/*",
            ],
            control,
        )?;
        git_until(&["-C", &dest_str, "checkout", "-q", commit], control)?;
    }
    Ok(())
}

fn ensure_bare_cache(
    cache: &Path,
    clone: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if cache.exists() {
        return Ok(());
    }
    let parent = cache
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cache has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".clone-")
        .tempdir_in(parent)?;
    let repository = staging.path().join("repository.git");
    clone(&repository)?;
    std::fs::rename(repository, cache)?;
    Ok(())
}

/// Runs only on the blocking worker, so Drop can synchronously reap the child
/// before the worker releases the repository lock or deletes its staging tree.
struct GitChild(Child);

impl GitChild {
    fn stop(&mut self) {
        #[cfg(unix)]
        unsafe {
            // Each command starts a fresh process group; include git helpers.
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &self.0.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for GitChild {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_command(command: &mut Command, control: &CheckoutControl) -> anyhow::Result<ExitStatus> {
    control.check()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = GitChild(command.spawn()?);
    loop {
        control.check()?;
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn git_until(args: &[&str], control: &CheckoutControl) -> anyhow::Result<()> {
    // File-backed stderr avoids blocking on full pipes and bounds the memory
    // needed for a noisy clone. Only a diagnostic tail is read on failure.
    let mut stderr = tempfile::tempfile()?;
    let status = run_command(
        Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr.try_clone()?),
        control,
    )?;
    if !status.success() {
        let length = stderr.metadata()?.len();
        stderr.seek(SeekFrom::Start(length.saturating_sub(1600)))?;
        let mut bytes = Vec::new();
        stderr.take(1600).read_to_end(&mut bytes)?;
        let reason = String::from_utf8_lossy(&bytes);
        let chars: Vec<char> = reason.trim().chars().collect();
        let tail: String = chars[chars.len().saturating_sub(400)..].iter().collect();
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            if tail.is_empty() {
                "(no stderr)"
            } else {
                &tail
            }
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn git(args: &[&str]) -> anyhow::Result<()> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    tokio::task::spawn_blocking(move || {
        git_until(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &CheckoutControl {
                cancelled,
                deadline: Instant::now() + Duration::from_secs(30),
            },
        )
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_repository_is_checked_out_through_a_complete_cache() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let source_str = source.to_string_lossy();
        git(&["init", "-q", &source_str]).await.unwrap();
        std::fs::write(source.join("tracked.txt"), "source content").unwrap();
        git(&["-C", &source_str, "add", "tracked.txt"])
            .await
            .unwrap();
        git(&[
            "-C",
            &source_str,
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "initial",
        ])
        .await
        .unwrap();
        let revision = Command::new("git")
            .args(["-C", &source_str, "rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(revision.status.success());
        let commit = String::from_utf8(revision.stdout).unwrap();
        let cache = root.path().join("cache.git");
        let destination = root.path().join("checkout");
        let worker_cache = cache.clone();
        let worker_dest = destination.clone();
        run_worker(cache.clone(), Duration::from_secs(10), move |control| {
            checkout_at(
                &source.to_string_lossy(),
                commit.trim(),
                &worker_dest,
                &worker_cache,
                &control,
            )
        })
        .await
        .unwrap();
        assert!(cache.join("HEAD").is_file());
        assert_eq!(
            std::fs::read_to_string(destination.join("tracked.txt")).unwrap(),
            "source content"
        );
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".clone-")
        }));
    }

    #[test]
    fn incomplete_clone_is_never_published() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache.git");
        let failed = ensure_bare_cache(&cache, |staging| {
            std::fs::create_dir(staging)?;
            std::fs::write(staging.join("partial"), "unfinished")?;
            anyhow::bail!("clone interrupted")
        });
        assert!(failed.is_err());
        assert!(!cache.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        ensure_bare_cache(&cache, |staging| {
            std::fs::create_dir(staging)?;
            std::fs::write(staging.join("complete"), "ready")?;
            assert!(!cache.exists());
            Ok(())
        })
        .unwrap();
        assert!(cache.join("complete").is_file());
    }

    #[cfg(unix)]
    async fn cancellation_stops_children_before_unlock(abort_caller: bool) {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache.git");
        let directory = root.path().to_path_buf();
        let task = tokio::spawn(run_worker(
            cache.clone(),
            Duration::from_millis(300),
            move |control| {
                run_command(Command::new("sh").args([
                "-c",
                "echo $$ > \"$1/leader\"; (sleep 0.6; echo late > \"$1/late\") & : > \"$1/started\"; wait",
                "checkout-test",
            ]).arg(directory), &control)?;
                Ok(())
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if abort_caller {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert!(
                task.await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
        }
        let lock = repo_lock_for(&cache);
        let _next_checkout = tokio::time::timeout(Duration::from_secs(5), lock.lock())
            .await
            .unwrap();
        let pid = std::fs::read_to_string(root.path().join("leader"))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "lock released before leader was reaped"
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !root.path().join("late").exists(),
            "descendant wrote after checkout stopped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reaps_git_before_releasing_the_repo_lock() {
        cancellation_stops_children_before_unlock(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_checkout_still_reaps_git_before_unlocking() {
        cancellation_stops_children_before_unlock(true).await;
    }
}
