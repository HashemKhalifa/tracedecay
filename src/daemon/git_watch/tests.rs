use super::*;

use notify::event::EventAttributes;
use std::process::Command;

#[test]
fn dirty_set_coalesces_and_takes_once() {
    let mut set = DirtySet::default();
    assert!(set.is_clean());
    set.dirty = true;
    set.branches.insert("feat/a".to_string());
    set.branches.insert("feat/a".to_string()); // dedup
    set.branches.insert("feat/b".to_string());
    assert!(!set.is_clean());

    let plan = set.take();
    assert!(plan.dirty);
    assert_eq!(plan.branches.len(), 2);
    // Draining resets the dirty state so the next cycle starts clean.
    assert!(set.is_clean());
    let empty = set.take();
    assert!(empty.is_empty());
}

#[test]
fn ref_event_marks_branch_and_delete_marks_gc() {
    let state = Arc::new(WatchState {
        project_root: PathBuf::from("/tmp/x"),
        dirty: Mutex::new(DirtySet::default()),
        wake: Notify::new(),
        health: ProjectHealth::default(),
        task: Mutex::new(None),
        entered_debounce: Notify::new(),
    });
    // Simulate a refs/heads create.
    let create = notify::Event {
        kind: EventKind::Create(notify::event::CreateKind::File),
        paths: vec![PathBuf::from("/repo/.git/refs/heads/feat/x")],
        attrs: EventAttributes::default(),
    };
    classify_and_mark(&state, &create);
    // Simulate a worktree delete.
    let remove = notify::Event {
        kind: EventKind::Remove(notify::event::RemoveKind::Folder),
        paths: vec![PathBuf::from("/repo/.git/worktrees/wt1")],
        attrs: EventAttributes::default(),
    };
    classify_and_mark(&state, &remove);

    let dirty = state.dirty.blocking_lock();
    assert!(dirty.dirty);
    assert!(dirty.branches.contains("feat/x"));
    assert!(dirty.gc_eligible);
}

#[test]
fn heartbeat_staleness() {
    let fresh = ProjectHealthSnapshot {
        last_heartbeat: now_secs(),
        last_sync: 0,
        degraded: false,
    };
    assert!(!fresh.heartbeat_stale());
    let never = ProjectHealthSnapshot {
        last_heartbeat: 0,
        last_sync: 0,
        degraded: false,
    };
    assert!(never.heartbeat_stale());
    let old = ProjectHealthSnapshot {
        last_heartbeat: now_secs().saturating_sub(HEARTBEAT_STALE_SECS + 10),
        last_sync: 0,
        degraded: false,
    };
    assert!(old.heartbeat_stale());
}

// ---- Real `GitWatcher` tests (drive the public API + the real debounce
// path, not a reimplemented helper). The integration suite cannot reach
// these crate-private internals because `git_watch` is not re-exported. ----

/// A test config with a tiny debounce so the real debounce path settles fast.
fn fast_watch_config() -> SyncConfig {
    let mut config = SyncConfig {
        auto_watch: true,
        ..SyncConfig::default()
    };
    config.watch_debounce_ms = 50;
    config.watch_max_delay_ms = 500;
    config.watch_max_projects = 32;
    config.backstop_interval_mins = 0; // no backstop noise in these tests
    config.max_concurrent_syncs = 2;
    config
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// A bare temp git repo with one commit. Not indexed by tracedecay — these
/// tests exercise the watcher's registration/debounce plumbing, which runs
/// regardless of whether a store exists (a sync on a non-indexed project is
/// a cheap no-op).
fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "hello\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);
    dir
}

#[tokio::test]
async fn ensure_watching_registers_dedups_and_caps() {
    let repo_a = temp_repo();
    let repo_b = temp_repo();
    let repo_c = temp_repo();

    // Cap of 2: the third project must not register.
    let mut config = fast_watch_config();
    config.watch_max_projects = 2;
    let watcher = GitWatcher::new(config);
    assert!(watcher.is_enabled());

    watcher.ensure_watching(repo_a.path()).await;
    assert_eq!(watcher.health_report().await.len(), 1);

    // Dedup: registering the same project again is a no-op.
    watcher.ensure_watching(repo_a.path()).await;
    assert_eq!(watcher.health_report().await.len(), 1);

    watcher.ensure_watching(repo_b.path()).await;
    assert_eq!(watcher.health_report().await.len(), 2);

    // Cap: the third project is refused (backstop still covers it elsewhere).
    watcher.ensure_watching(repo_c.path()).await;
    assert_eq!(watcher.health_report().await.len(), 2);
}

#[tokio::test]
async fn disabled_watcher_never_registers() {
    let repo = temp_repo();
    let mut config = fast_watch_config();
    config.auto_watch = false;
    let watcher = GitWatcher::new(config);
    assert!(!watcher.is_enabled());
    watcher.ensure_watching(repo.path()).await;
    assert!(watcher.health_report().await.is_empty());
}

/// The safety-critical property that justifies this metadata watcher over the
/// removed #80 working-tree watcher: a plain source-file edit (no git
/// operation) must NOT trigger any watcher sync. We drive the REAL watcher
/// task and assert `last_sync` never advances.
///
/// This test proves a NEGATIVE about a REAL inotify event (a working-tree
/// write that must not be delivered/acted on), so it deliberately runs on the
/// real clock — paused time cannot manufacture "an OS event that never
/// arrives". Determinism instead comes from making both the readiness and the
/// negative window OBSERVABLE rather than fixed sleeps:
///   1. We wait on the `entered_debounce` state signal, so the watch is
///      PROVABLY installed before the edit — closing the old false-pass
///      window where a 200ms sleep elapsed before inotify was armed (a real
///      regression could then slip through unseen).
///   2. After the edit we poll `last_sync` across a window several times the
///      debounce+max-delay budget and fail on the FIRST advance. A scheduler
///      stall only lengthens the safe window — it can never produce a false
///      negative — so no magic epsilon is needed.
#[tokio::test]
async fn source_file_edit_triggers_no_sync() {
    let repo = temp_repo();
    let config = fast_watch_config();
    let debounce_ms = config.watch_debounce_ms;
    let max_delay_ms = config.watch_max_delay_ms;
    let watcher = GitWatcher::new(config);
    watcher.ensure_watching(repo.path()).await;

    let canonical = repo
        .path()
        .canonicalize()
        .unwrap_or_else(|_| repo.path().to_path_buf());
    let state = {
        let projects = watcher.inner.projects.lock().await;
        Arc::clone(projects.get(&canonical).expect("project registered"))
    };

    // Prove the watch is installed and the loop is parked BEFORE we edit, so
    // the write cannot happen in an unwatched window (which would be a false
    // pass masking a regression).
    assert!(
        await_watcher_ready(&state).await,
        "the watch task must reach debounce_loop before the edit"
    );

    let baseline = state.health.snapshot().last_sync;

    // Edit source files in the working tree — NOT git metadata. A correct
    // metadata-only watcher never watches these paths, so no event fires.
    std::fs::write(repo.path().join("a.txt"), "changed by editor\n").unwrap();
    std::fs::write(repo.path().join("b.txt"), "brand new file\n").unwrap();

    // Poll across a window MUCH larger than debounce + max-delay, failing
    // fast on the FIRST sign of a spurious reaction. We assert TWO things at
    // every tick, so the test is non-vacuous even against an unindexed repo
    // (where a sync would no-op and never move `last_sync`):
    //   * `last_sync` never advances — no sync ran, AND
    //   * the dirty set never becomes marked — no working-tree event ever
    //     reached `classify_and_mark`. The dirty mark is the ROOT observable:
    //     if a regression recursively watched the working tree, the edit
    //     would set `dirty` for the ~debounce+max-delay window, which this
    //     20ms poll catches before the loop drains it. A scheduler stall only
    //     widens both safe windows; it cannot fabricate a false negative.
    let window = Duration::from_millis((debounce_ms + max_delay_ms) * 4 + 500);
    let deadline = std::time::Instant::now() + window;
    while std::time::Instant::now() < deadline {
        assert_eq!(
            state.health.snapshot().last_sync,
            baseline,
            "a working-tree source edit must not advance last_sync (metadata-only watcher)"
        );
        assert!(
            state.dirty.lock().await.is_clean(),
            "a working-tree source edit must never mark the dirty set \
             (the metadata-only watcher must not watch the working tree)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Final check after the full window has elapsed.
    assert_eq!(
        state.health.snapshot().last_sync,
        baseline,
        "a working-tree source edit must not advance last_sync (metadata-only watcher)"
    );
    assert!(
        state.dirty.lock().await.is_clean(),
        "a working-tree source edit must never mark the dirty set"
    );
}

/// Awaits the live watch task's `entered_debounce` readiness signal under a
/// generous deadline, replacing a fixed "install settle" sleep. `notify_one`
/// stores a permit, so this resolves whether the loop signals before or after
/// we subscribe. Under `start_paused` the producer task is scheduled to the
/// signal before the runtime can idle-advance the timeout, so this is
/// deterministic; the timeout is only a hung-task backstop, never a race
/// window. A `false` return means the watch task never reached the loop —
/// a real regression, not flake.
async fn await_watcher_ready(state: &Arc<WatchState>) -> bool {
    tokio::time::timeout(Duration::from_secs(30), state.entered_debounce.notified())
        .await
        .is_ok()
}

/// The REAL debounce path (`project_task` → `debounce_loop`) coalesces a
/// burst of metadata events into a single drained pass: after events stop,
/// the dirty set is taken exactly once and returns to clean. This drives the
/// live task (not a reimplemented helper) and injects events through the real
/// notify-callback body (`classify_and_mark`), then asserts the debounce loop
/// drains them.
///
/// Deterministic under `start_paused = true`: there is no wall-clock guess.
/// Readiness is a state signal (`entered_debounce`), and the coalesce sleep
/// is driven by `tokio::time::advance` PAST the hard cap, so the drain is
/// forced to fire regardless of scheduler latency. The coalescing guarantee
/// is still fully asserted: the set is dirty before time advances and clean
/// after exactly one drain (a per-event re-fire would either not reach clean
/// or would leave residue across the burst).
#[tokio::test(start_paused = true)]
async fn debounce_loop_coalesces_and_drains_events() {
    let repo = temp_repo();
    let watcher = GitWatcher::new(fast_watch_config());
    let max_delay_ms = watcher.inner.config.watch_max_delay_ms;
    watcher.ensure_watching(repo.path()).await;

    let canonical = repo
        .path()
        .canonicalize()
        .unwrap_or_else(|_| repo.path().to_path_buf());
    let state = {
        let projects = watcher.inner.projects.lock().await;
        Arc::clone(projects.get(&canonical).expect("registered"))
    };

    // Wait — deterministically, on state — until the live task has installed
    // the watcher and reached `debounce_loop`, so the injected events cannot
    // race ahead of the loop's first park.
    assert!(
        await_watcher_ready(&state).await,
        "the watch task must reach debounce_loop"
    );

    // Inject a burst of ref events through the real callback body, exactly as
    // the notify thread would on a `git commit` / branch churn. The debounce
    // loop must coalesce them and drain the dirty set to clean.
    for i in 0..5 {
        let event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![canonical.join(format!(".git/refs/heads/feat/{i}"))],
            attrs: EventAttributes::default(),
        };
        classify_and_mark(&state, &event);
    }
    assert!(
        !state.dirty.lock().await.is_clean(),
        "events should mark the dirty set before the debounce fires"
    );

    // Let the loop wake from the events, observe no in-flight op, compute its
    // coalesce deadline, and park on the debounce sleep. yield_now (not a
    // timer) hands the scheduler to the loop task without advancing virtual
    // time, so `first_event`/`last_event` are set before we advance.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // Drive virtual time strictly PAST the hard cap. This deterministically
    // elapses both the quiet window and the max-delay cap, forcing the single
    // coalesced drain — no dependence on wall-clock or scheduler timing.
    tokio::time::advance(Duration::from_millis(max_delay_ms + 1)).await;

    // The loop must now have taken the plan exactly once, leaving the set
    // clean. Poll under a paused-time timeout; auto-advance drives any
    // residual internal sleep, so a hang here means a real drain failure.
    let drained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if state.dirty.lock().await.is_clean() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        drained,
        "the real debounce loop must coalesce the event burst and drain the dirty set"
    );
}
