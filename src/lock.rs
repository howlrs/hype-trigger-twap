//! Single-writer advisory lock + durable nonce high-water-mark (Issue #5).
//!
//! ## Why
//!
//! `HlClient`'s nonce (`src/client.rs`) is a process-local `AtomicU64` that
//! resets to 0 on every restart. Two live processes started with the same
//! `HL_AGENT_PK` (or one process restarted while an older one is still
//! running) would each mint their own independent nonce sequence and could
//! both execute the same target in full — nonce collisions aside, this is a
//! double-execution hazard the journal (Issue #4) does not protect against,
//! because the journal partitions state by run, not by agent/network across
//! runs.
//!
//! This module closes both gaps for the single-host case:
//!
//! 1. [`ProcessLock`] — an advisory `flock`-semantics file lock (via
//!    `fd-lock`), keyed by `network + agent_address`, taken once at live
//!    startup before any order (and before Issue #4's incomplete-run
//!    reconciliation runs) so a second live writer for the same key fails
//!    fast instead of racing the first.
//! 2. [`NonceHwm`] — a durable high-water mark for the nonce, keyed the same
//!    way, so the nonce sequence is monotone across process restarts (and
//!    across a backward system clock jump), not just within one process's
//!    lifetime.
//!
//! ## Design: why a free-standing module, not `HlClient` internals
//!
//! `HlClient::new(config, signer)` is used, unmodified, by essentially every
//! existing test in `src/client.rs` (`AtomicU64::new(0)` seed). Changing its
//! signature to take a persistence path would ripple through all of them for
//! a concern (state-dir I/O) that has nothing to do with what `HlClient`
//! itself models (HTTP + signing). Instead, `main.rs` owns a [`NonceHwm`]
//! alongside the `HlClient` it already constructs: it reads the persisted
//! HWM (if any), computes `seed = max(now_ms, hwm + 1)`, and calls
//! [`HlClient::seed_nonce`] with `seed - 1` so the client's own
//! `next_nonce()` (`max(last + 1, now_ms)`) naturally produces `seed` on its
//! first call — the existing in-process monotonicity logic in `client.rs` is
//! reused unchanged, this module only supplies a durable floor for it.
//!
//! `HlClient` itself calls [`NonceHwm::advance`] after every nonce its
//! private `next_nonce()` mints (once a [`NonceHwm`] has been installed via
//! [`HlClient::seed_nonce`]) so the on-disk HWM never falls behind what has
//! already been signed into a request — see `next_nonce`'s doc comment in
//! `src/client.rs` for exactly where this hook fires.
//!
//! ## Durability: fsync on every mint, matching `journal.rs`
//!
//! Every [`NonceHwm::advance`] call writes the new value and calls
//! `sync_data()` before returning, exactly like
//! [`crate::journal::ExecutionJournal::record`] fsyncs every journal record.
//! The same tradeoff analysis applies: a nonce, once minted and signed into
//! a request that might reach the exchange, must never be reusable by a
//! restarted process — an un-fsynced HWM write that is lost on crash would
//! let a restart replay a nonce a still-in-flight (or already-accepted)
//! request used, which is exactly the ambiguity Issue #4's journal exists to
//! avoid for order intent. TWAP slices are seconds-to-minutes apart, not a
//! hot loop, so one fsync per mint is not a throughput concern here.
//!
//! ## What this does NOT protect against
//!
//! The lock is a local `flock` — it has no visibility across hosts. Two
//! hosts running the same agent key concurrently are NOT caught by this
//! lock; the only real boundary there is "one dedicated API wallet per
//! trading process," which `docs/OPERATIONS.md` documents as a hard
//! requirement, not a suggestion. See that doc for the full multi-host
//! guidance and what would be required (an external nonce coordinator) if
//! HWM/lock sharing across processes were ever intentionally allowed.
//!
//! ## Extension point for Task 9 #1 (passive/post-only)
//!
//! Task 9's passive/post-only mode runs inside the same live startup path
//! (`run_with_cli`) that this module's lock acquisition point sits in. It
//! does not change anything about lock/HWM semantics — one live process
//! still equals one lock holder regardless of order style — so no
//! extension is anticipated here; noted for completeness alongside
//! `journal.rs`'s equivalent note.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Errors from lock/HWM I/O. Kept separate from [`crate::errors::HlError`]
/// for the same reason [`crate::journal::JournalError`] is: local filesystem
/// failures, not HL communication errors. Callers fold these into
/// `Result<_, String>` via `.to_string()`, matching every other startup
/// failure path in `main.rs`.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("lock I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("lock serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// A second writer for the same network+agent already holds the lock.
    #[error(
        "another live process already holds the writer lock for this network+agent \
         (lock file: {path}). If that process is gone, its lock is released automatically \
         on process death (this is a real flock, not just the metadata file) — if you are \
         certain no other process for this network+agent is running, check the metadata file \
         for a stale PID before retrying. Nothing was sent by this process."
    )]
    AlreadyLocked { path: PathBuf },
}

/// Stable partition key for the lock/HWM files: `network:agent_address`.
/// Deliberately mirrors [`crate::journal::RunHeader::run_key`]'s format,
/// but as a free function usable before a `RunHeader` exists — at live
/// startup, the lock must be taken before a `run_id`/journal (and therefore
/// before any `RunHeader`) exist at all.
pub fn lock_key(network: &str, agent: &crate::types::Address) -> String {
    format!("{network}:{}", agent.as_str())
}

/// Filesystem-safe encoding of a [`lock_key`] value for use as a filename
/// component (replaces `:` and `/`, both of which are meaningful/illegal in
/// path segments on at least one common OS, with `_`).
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c == ':' || c == '/' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Diagnostics-only metadata written beside the lock file. This is NOT the
/// safety mechanism (the `flock` itself, which auto-releases on process
/// death, is) — it exists purely so a human investigating a lock-acquisition
/// failure can see which PID/run holds it without needing `lsof`/`fuser`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockMetadata {
    pub pid: u32,
    pub started_at_unix_ms: u64,
    /// Present once the run has allocated a `run_id` (the lock is taken
    /// before that, so this is `None` at acquisition time and callers may
    /// update the metadata file after the run_id is known — current callers
    /// leave it `None`, documented here as a forward-compatible field rather
    /// than removed, since a future caller may choose to rewrite the file).
    pub run_id: Option<String>,
    /// Free-text summary of what this run intends to do (symbol/side/size),
    /// for a human reading the metadata file during stale-lock triage.
    pub plan_summary: String,
}

impl LockMetadata {
    pub fn new(plan_summary: impl Into<String>) -> Self {
        Self {
            pid: std::process::id(),
            started_at_unix_ms: crate::twap::wall_clock_now_ms(),
            run_id: None,
            plan_summary: plan_summary.into(),
        }
    }
}

/// Held single-writer lock for one `network+agent`. Dropping this releases
/// the underlying `flock` (via `fd_lock::RwLock`'s own `Drop`), which is the
/// real safety mechanism — the metadata JSON beside it is diagnostics only.
pub struct ProcessLock {
    // `fd_lock::RwLock` releases the flock in its own Drop when this field
    // is dropped; kept alive for the lifetime of `ProcessLock` for exactly
    // that reason. Boxed to keep `ProcessLock` a stable-sized, movable type
    // regardless of the guard's internal representation.
    _guard: fd_lock::RwLock<File>,
    lock_path: PathBuf,
}

impl ProcessLock {
    /// Path to the lock file for a given state root + key.
    pub fn lock_path(state_root: &Path, key: &str) -> PathBuf {
        state_root
            .join("locks")
            .join(format!("{}.lock", sanitize_key(key)))
    }

    /// Path to the diagnostics metadata file beside the lock file.
    pub fn metadata_path(state_root: &Path, key: &str) -> PathBuf {
        state_root
            .join("locks")
            .join(format!("{}.meta.json", sanitize_key(key)))
    }

    /// Acquire the lock for `key` under `state_root`, creating
    /// `<state_root>/locks/` if needed, and writing `metadata` beside it.
    /// Fails fast (non-blocking `try_write`) if another holder — in this
    /// process or another — already holds it: this is a fail-fast
    /// single-writer guarantee, not a wait-for-availability queue.
    pub fn acquire(
        state_root: &Path,
        key: &str,
        metadata: &LockMetadata,
    ) -> Result<Self, LockError> {
        let lock_dir = state_root.join("locks");
        std::fs::create_dir_all(&lock_dir)?;
        let lock_path = Self::lock_path(state_root, key);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        let mut rw_lock = fd_lock::RwLock::new(file);
        // `fd_lock`'s guard's `Drop` impl releases the flock (`LOCK_UN`), so
        // we must NOT let the guard itself drop while we want the lock held.
        // The guard borrows `rw_lock` mutably, which makes storing both the
        // `RwLock` and a guard borrowing it in the same struct impossible in
        // safe Rust without self-referential storage. Since flock state
        // lives on the open file description (not on the guard object), the
        // fix is simple: `try_write` to prove/take the lock, then
        // `mem::forget` the guard immediately — this skips its `Drop` (no
        // unlock) while the underlying fd (owned by `rw_lock`, kept in
        // `self`) stays open for `ProcessLock`'s lifetime, keeping the OS
        // lock held until `ProcessLock` itself is dropped and closes the fd.
        match rw_lock.try_write() {
            Ok(guard) => std::mem::forget(guard),
            Err(_) => return Err(LockError::AlreadyLocked { path: lock_path }),
        }
        let meta_path = Self::metadata_path(state_root, key);
        let meta_json = serde_json::to_vec_pretty(metadata)?;
        let mut meta_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&meta_path)?;
        meta_file.write_all(&meta_json)?;
        meta_file.sync_data()?;
        Ok(Self {
            _guard: rw_lock,
            lock_path,
        })
    }

    pub fn lock_path_ref(&self) -> &Path {
        &self.lock_path
    }
}

/// Durable per-`network+agent` nonce high-water mark.
///
/// `next_seed()` computes `max(now_ms, hwm + 1)` from the persisted value
/// (0 if no file exists yet — the very first run for this key). Callers
/// (`main.rs`) use this to seed `HlClient`'s in-process nonce counter; every
/// nonce actually minted afterwards must be reported back via
/// [`NonceHwm::advance`] so the persisted HWM never falls behind what has
/// already been signed into a request.
pub struct NonceHwm {
    path: PathBuf,
    current: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NonceHwmFile {
    hwm: u64,
}

impl NonceHwm {
    pub fn hwm_path(state_root: &Path, key: &str) -> PathBuf {
        state_root
            .join("locks")
            .join(format!("{}.nonce-hwm.json", sanitize_key(key)))
    }

    /// Load the persisted HWM for `key` under `state_root` (0 if absent —
    /// first run), and compute the seed nonce = `max(now_ms, hwm + 1)`. This
    /// constructor does NOT write anything; callers of `next_seed`/`advance`
    /// perform the writes, keeping "resolve a value" and "make it durable"
    /// separate so a read-only caller could inspect a HWM without touching
    /// disk (though no current caller does this in read-only mode — the
    /// lock/HWM machinery is entirely gated on live mode in `main.rs`).
    pub fn load(state_root: &Path, key: &str) -> Result<Self, LockError> {
        let path = Self::hwm_path(state_root, key);
        let hwm = match std::fs::read(&path) {
            Ok(bytes) => {
                let parsed: NonceHwmFile = serde_json::from_slice(&bytes)?;
                parsed.hwm
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, current: hwm })
    }

    /// `max(now_ms, hwm + 1)` — the seed nonce this process should start
    /// from. Monotone across restart (uses the persisted `hwm`) and across a
    /// backward clock jump (the `+1` floor beats a `now_ms` that has gone
    /// backwards below `hwm`).
    pub fn next_seed(&self, now_ms: u64) -> u64 {
        now_ms.max(self.current + 1)
    }

    /// Record that `nonce` has been minted (signed into a request): persist
    /// it as the new HWM if it exceeds the current one, `fsync`ing
    /// immediately (see module doc for why every mint is fsynced, not
    /// batched). A no-op write is skipped if `nonce <= current` (should not
    /// happen given `next_seed`'s contract, but kept defensive rather than
    /// panicking — `next_seed`/`advance` misuse should not be able to
    /// silently move the HWM backwards).
    ///
    /// B3 hardening, two independent fixes:
    ///
    /// (a) The in-memory `self.current` is now updated only AFTER the
    /// durable write (including its `fsync`/rename) has fully succeeded. The
    /// old code set `self.current = nonce` BEFORE attempting the write; if
    /// that write then failed, the in-memory value had already advanced, so
    /// a LATER `advance` call with the same (now already-in-memory) nonce
    /// would short-circuit via the `nonce <= self.current` no-op check above
    /// and never retry the write — leaving the on-disk HWM permanently
    /// stale relative to what was actually signed, which risks nonce reuse
    /// after a restart. On error, `self.current` now stays exactly as it
    /// was, so a subsequent `advance` call with the same or a larger nonce
    /// is free to retry the write.
    ///
    /// (b) The write itself is now tmp-file + `fsync` + atomic `rename`,
    /// not a truncate-in-place `write_all` on the live path. A crash
    /// mid-write under the old scheme could leave a truncated/partial JSON
    /// file that fails to parse on the next `NonceHwm::load`, which would
    /// block a live run from starting at all (availability). `rename(2)` on
    /// the same filesystem is atomic — the live path either still has its
    /// old complete contents, or is fully replaced by the new complete
    /// contents; there is no observable partial state.
    pub fn advance(&mut self, nonce: u64) -> Result<(), LockError> {
        if nonce <= self.current {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec(&NonceHwmFile { hwm: nonce })?;

        // Atomic tmp-file + fsync + rename (B3b). The tmp path lives beside
        // the target (same directory => same filesystem => `rename` is
        // atomic) and is named uniquely enough that two processes racing
        // `advance` for the SAME key (which should never happen — the
        // caller holds `ProcessLock` for this key for the run's whole
        // lifetime — but kept defensive) do not clobber each other's
        // in-flight tmp file.
        let tmp_path = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        f.write_all(&body)?;
        f.sync_data()?;
        drop(f);
        std::fs::rename(&tmp_path, &self.path)?;

        // Only now, after the durable write has fully succeeded, does the
        // in-memory HWM advance (B3a).
        self.current = nonce;
        Ok(())
    }

    /// Currently persisted HWM (test/diagnostic accessor).
    pub fn current(&self) -> u64 {
        self.current
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("hype-twap-lock-test-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn addr(s: &str) -> crate::types::Address {
        crate::types::Address::new(s)
    }

    // === lock ===

    #[test]
    fn lock_key_matches_run_key_format() {
        let a = addr("0xabc");
        assert_eq!(lock_key("testnet", &a), "testnet:0xabc");
    }

    #[test]
    fn second_acquire_for_same_key_fails_fast() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        let meta = LockMetadata::new("first holder");
        let _first =
            ProcessLock::acquire(tmp.path(), &key, &meta).expect("first acquire must succeed");

        let meta2 = LockMetadata::new("second holder");
        let second = ProcessLock::acquire(tmp.path(), &key, &meta2);
        match second {
            Err(LockError::AlreadyLocked { .. }) => {}
            Err(other) => panic!("expected AlreadyLocked, got a different error: {other}"),
            Ok(_) => panic!("second acquire for the same key must fail fast, but it succeeded"),
        }
    }

    #[test]
    fn different_key_can_be_acquired_concurrently() {
        let tmp = TempDir::new();
        let key1 = lock_key("testnet", &addr("0xabc"));
        let key2 = lock_key("testnet", &addr("0xdef"));
        let _l1 = ProcessLock::acquire(tmp.path(), &key1, &LockMetadata::new("a"))
            .expect("first key acquires fine");
        let _l2 = ProcessLock::acquire(tmp.path(), &key2, &LockMetadata::new("b"))
            .expect("different agent must not be blocked by the first lock");
    }

    #[test]
    fn different_network_same_agent_can_be_acquired_concurrently() {
        let tmp = TempDir::new();
        let key1 = lock_key("testnet", &addr("0xabc"));
        let key2 = lock_key("mainnet", &addr("0xabc"));
        let _l1 = ProcessLock::acquire(tmp.path(), &key1, &LockMetadata::new("a")).unwrap();
        let _l2 = ProcessLock::acquire(tmp.path(), &key2, &LockMetadata::new("b"))
            .expect("different network must not be blocked by the first lock");
    }

    #[test]
    fn releasing_the_lock_allows_reacquisition() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        {
            let _l1 = ProcessLock::acquire(tmp.path(), &key, &LockMetadata::new("a")).unwrap();
            // dropped at end of this block
        }
        let _l2 = ProcessLock::acquire(tmp.path(), &key, &LockMetadata::new("b"))
            .expect("lock must be reacquirable once the holder drops");
    }

    #[test]
    fn acquire_writes_readable_metadata_json_beside_the_lock() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        let meta = LockMetadata::new("HYPE long 100usd 4 slices");
        let _l = ProcessLock::acquire(tmp.path(), &key, &meta).unwrap();

        let meta_path = ProcessLock::metadata_path(tmp.path(), &key);
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        let parsed: LockMetadata = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.pid, std::process::id());
        assert_eq!(parsed.plan_summary, "HYPE long 100usd 4 slices");
    }

    // === nonce hwm ===

    #[test]
    fn first_run_seeds_from_now_ms_when_no_hwm_file_exists() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        let hwm = NonceHwm::load(tmp.path(), &key).unwrap();
        assert_eq!(hwm.current(), 0);
        assert_eq!(hwm.next_seed(1_000_000), 1_000_000);
    }

    #[test]
    fn restart_monotonicity_seed_exceeds_persisted_hwm_even_if_clock_is_behind() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));

        // First "process": mints a nonce far in the future (simulating a
        // clock that was briefly fast, or simply the last nonce minted).
        let mut hwm1 = NonceHwm::load(tmp.path(), &key).unwrap();
        hwm1.advance(5_000_000).unwrap();
        drop(hwm1);

        // Second "process" (restart): loads fresh from disk. Even though
        // `now_ms` here is smaller than the persisted HWM, next_seed must
        // still exceed it.
        let hwm2 = NonceHwm::load(tmp.path(), &key).unwrap();
        assert_eq!(hwm2.current(), 5_000_000);
        let seed = hwm2.next_seed(1_000_000); // now_ms "behind" the HWM
        assert!(
            seed > 5_000_000,
            "seed ({seed}) must exceed the persisted HWM (5_000_000) across restart"
        );
        assert_eq!(seed, 5_000_001);
    }

    #[test]
    fn clock_rollback_monotonicity_seed_still_exceeds_hwm() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));

        let mut hwm = NonceHwm::load(tmp.path(), &key).unwrap();
        // Simulate several nonces minted at a "normal" clock value.
        hwm.advance(10_000_000).unwrap();
        hwm.advance(10_000_050).unwrap();
        assert_eq!(hwm.current(), 10_000_050);

        // Now simulate the system clock rolling backward (e.g. NTP
        // correction) within the SAME process — next_seed must still
        // produce a value strictly greater than the last advanced HWM.
        let rolled_back_now_ms = 9_000_000;
        let seed = hwm.next_seed(rolled_back_now_ms);
        assert!(
            seed > 10_000_050,
            "seed ({seed}) must exceed the HWM (10_000_050) despite clock rollback to {rolled_back_now_ms}"
        );
        assert_eq!(seed, 10_000_051);
    }

    #[test]
    fn advance_never_moves_the_hwm_backwards() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        let mut hwm = NonceHwm::load(tmp.path(), &key).unwrap();
        hwm.advance(100).unwrap();
        hwm.advance(50).unwrap(); // smaller than current: no-op
        assert_eq!(hwm.current(), 100);
    }

    #[test]
    fn advance_persists_to_disk_and_is_visible_after_reload() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        let mut hwm = NonceHwm::load(tmp.path(), &key).unwrap();
        hwm.advance(777).unwrap();
        drop(hwm);

        let reloaded = NonceHwm::load(tmp.path(), &key).unwrap();
        assert_eq!(reloaded.current(), 777);
    }

    // === B3: advance's memory/disk ordering and write atomicity ===

    /// B3(a): if the durable write fails, `self.current` (the in-memory
    /// HWM) must NOT have been advanced — otherwise a later `advance` call
    /// with the SAME (now already-in-memory) nonce short-circuits via the
    /// `nonce <= self.current` no-op check and never retries the write, so
    /// after a process restart the on-disk HWM is stale relative to what
    /// was actually signed, risking nonce reuse.
    ///
    /// Forces the write to fail by making the HWM file's parent directory
    /// (`<state_root>/locks`) read-only after it has already been created,
    /// so `OpenOptions::open(&self.path)` fails with a permission error —
    /// Unix-only (chmod), matches this crate's existing Unix-first stance
    /// elsewhere.
    #[cfg(unix)]
    #[test]
    fn advance_does_not_move_the_in_memory_hwm_when_the_durable_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));

        let mut hwm = NonceHwm::load(tmp.path(), &key).unwrap();
        // First advance succeeds normally and creates the `locks` dir.
        hwm.advance(100).unwrap();
        assert_eq!(hwm.current(), 100);

        // The write path creates a NEW tmp file (for the atomic tmp+rename
        // scheme), which needs write+execute permission on the PARENT
        // directory, not on any existing file — so make the `locks`
        // directory itself read+execute-only to force the tmp-file create
        // to fail.
        let locks_dir = tmp.path().join("locks");
        let original_mode = std::fs::metadata(&locks_dir).unwrap().permissions().mode();
        std::fs::set_permissions(&locks_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = hwm.advance(200);

        // Restore permissions before any panic-unwind cleanup (Drop) tries
        // to remove the tree, and so the retry below can write again.
        std::fs::set_permissions(&locks_dir, std::fs::Permissions::from_mode(original_mode))
            .unwrap();

        assert!(
            result.is_err(),
            "the write must actually fail under a read-only locks dir for this test to prove \
             anything"
        );
        assert_eq!(
            hwm.current(),
            100,
            "a failed durable write must NOT have advanced the in-memory HWM — otherwise a \
             later advance(200) call would short-circuit as a no-op (200 <= self.current) \
             without ever retrying the write, leaving the on-disk HWM stale at 100"
        );

        // The retry must actually be attempted (not silently skipped) once
        // the write path works again, and it must succeed.
        hwm.advance(200).unwrap();
        assert_eq!(hwm.current(), 200);
        let reloaded = NonceHwm::load(tmp.path(), &key).unwrap();
        assert_eq!(
            reloaded.current(),
            200,
            "after the retry succeeds, disk must reflect the advanced HWM"
        );
    }

    /// B3(b): the HWM file write must be atomic (tmp-file + fsync + rename),
    /// not a truncate-in-place, so a crash mid-write cannot leave a
    /// corrupted/partial JSON file that fails to parse on the next startup
    /// (an availability bug: `NonceHwm::load` would error and block a live
    /// run entirely). This test simulates the "recovery" side of that
    /// property: after a normal `advance` completes, the file on disk must
    /// be a single complete, parbeable JSON document (proving the write
    /// path does not leave any intermediate/partial state visible) — and
    /// specifically that a `.tmp` sibling file used during the atomic
    /// rename does not leak/remain after a successful write.
    #[test]
    fn advance_write_is_atomic_tmp_file_is_not_left_behind_after_success() {
        let tmp = TempDir::new();
        let key = lock_key("testnet", &addr("0xabc"));
        let mut hwm = NonceHwm::load(tmp.path(), &key).unwrap();
        hwm.advance(42).unwrap();

        let hwm_path = NonceHwm::hwm_path(tmp.path(), &key);
        // The file itself must parse cleanly as a complete JSON document.
        let raw = std::fs::read_to_string(&hwm_path).unwrap();
        let parsed: NonceHwmFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.hwm, 42);

        // No leftover temp file from the atomic rename should remain beside
        // it.
        let dir = hwm_path.parent().unwrap();
        let leftover_tmp: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(
            leftover_tmp.is_empty(),
            "no .tmp file should remain after a successful atomic write: {leftover_tmp:?}"
        );
    }
}
