// Based on the parallel walking implementation from the `ignore` crate
// (https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore), version
// 0.4.25. The original implementation is licensed under Unlicense OR MIT.
//
// This module adapts the work-stealing traversal shape for Pixi's indexed VFS:
// workers can consult indexed directories for index-only traversal, or collect
// disk-walk deltas that are committed to the index once after traversal.

use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use crossbeam_deque::{Stealer, Worker as Deque};
use glob::Pattern;
use parking_lot::Mutex;

use crate::{
    GlobMTime, Inner, MatchEntry, VfsError, WalkDiagnostics, WalkMode, glob_matches,
    index::{DirId, FileId, Index, VfsEntryKind},
    latest_from_entries, node_as_dir, node_as_file,
};

#[derive(Clone)]
pub(crate) struct WalkPlan {
    pub(crate) root: PathBuf,
    pub(crate) pattern: Pattern,
    start_dir: PathBuf,
    max_descend_from_start: Option<usize>,
}

impl WalkPlan {
    pub(crate) fn new(root: PathBuf, pattern_text: String) -> Result<Self, VfsError> {
        let pattern = Pattern::new(&pattern_text).map_err(|source| VfsError::GlobPattern {
            pattern: pattern_text.clone(),
            source,
        })?;
        let analysis = PatternAnalysis::new(&pattern_text);
        Ok(Self {
            start_dir: root.join(&analysis.literal_prefix),
            root,
            pattern,
            max_descend_from_start: analysis.max_descend_from_start,
        })
    }

    fn should_descend(&self, depth_from_start: usize) -> bool {
        self.max_descend_from_start
            .is_none_or(|max| depth_from_start < max)
    }
}

#[derive(Debug)]
struct PatternAnalysis {
    /// Prefix before the first glob metacharacter. Starting the walk here lets
    /// literal patterns avoid scanning unrelated sibling directories.
    literal_prefix: PathBuf,
    /// Maximum depth to descend from `literal_prefix` for non-recursive
    /// patterns. `None` means the pattern contains `**` and may match at any
    /// depth below the start directory.
    max_descend_from_start: Option<usize>,
}

impl PatternAnalysis {
    fn new(pattern: &str) -> Self {
        let normalized = pattern.replace('\\', "/");
        let components: Vec<_> = normalized
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .collect();

        let first_meta = components.iter().position(|component| has_meta(component));
        let literal_prefix_components = match first_meta {
            Some(index) => &components[..index],
            None if components.is_empty() => &components[..],
            None => &components[..components.len().saturating_sub(1)],
        };
        let literal_prefix = literal_prefix_components.iter().collect::<PathBuf>();

        let has_recursive = components.iter().any(|component| *component == "**");
        let max_descend_from_start = if has_recursive {
            None
        } else {
            let dir_components_before_leaf = components.len().saturating_sub(1);
            Some(dir_components_before_leaf.saturating_sub(literal_prefix_components.len()))
        };

        Self {
            literal_prefix,
            max_descend_from_start,
        }
    }
}

fn has_meta(component: &str) -> bool {
    component.contains('*')
        || component.contains('?')
        || component.contains('[')
        || component.contains('{')
}

pub(crate) struct WalkOutcome {
    pub(crate) matched_files: HashSet<FileId>,
    pub(crate) max_by_mtime: BTreeSet<MatchEntry>,
}

impl WalkOutcome {
    fn new() -> Self {
        Self {
            matched_files: HashSet::new(),
            max_by_mtime: BTreeSet::new(),
        }
    }

    fn push_match(&mut self, file: FileId, index: &Index) {
        self.matched_files.insert(file);
        if let Some(entry) = MatchEntry::from_file(file, index) {
            self.max_by_mtime.insert(entry);
        }
    }

    pub(crate) fn current(&self, index: &Index) -> GlobMTime {
        latest_from_entries(&self.max_by_mtime, index)
    }

    fn merge(&mut self, mut other: WalkOutcome) {
        self.matched_files.extend(other.matched_files.drain());
        self.max_by_mtime.append(&mut other.max_by_mtime);
    }
}

#[derive(Default)]
pub(crate) struct DiskDelta {
    /// Directories that were actually visited by the backend walk. They are
    /// committed as unknown listings because the glob walk may intentionally
    /// skip non-matching files.
    pub(crate) visited_dirs: Vec<PathBuf>,
    /// Files that matched the query and should be stored in the query index.
    pub(crate) matched_files: Vec<MatchedFileDelta>,
}

impl DiskDelta {
    fn merge(&mut self, mut other: Self) {
        self.visited_dirs.append(&mut other.visited_dirs);
        self.matched_files.append(&mut other.matched_files);
    }
}

pub(crate) struct MatchedFileDelta {
    pub(crate) path: PathBuf,
    pub(crate) kind: VfsEntryKind,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<SystemTime>,
}

#[derive(Clone)]
struct DiskWork {
    dir: PathBuf,
    depth_from_start: usize,
}

#[derive(Clone)]
struct IndexWork {
    dir: DirId,
    depth_from_start: usize,
}

enum Message<W> {
    Work(W),
    Quit,
}

#[derive(Debug)]
struct Stack<W> {
    index: usize,
    deque: Deque<Message<W>>,
    stealers: Arc<[Stealer<Message<W>>]>,
}

impl<W> Stack<W> {
    fn new_for_each_thread(threads: usize, init: Vec<Message<W>>) -> Vec<Self> {
        let deques: Vec<Deque<Message<W>>> = std::iter::repeat_with(Deque::new_lifo)
            .take(threads)
            .collect();
        let stealers = Arc::<[Stealer<Message<W>>]>::from(
            deques.iter().map(Deque::stealer).collect::<Vec<_>>(),
        );
        let stacks: Vec<Stack<W>> = deques
            .into_iter()
            .enumerate()
            .map(|(index, deque)| Stack {
                index,
                deque,
                stealers: stealers.clone(),
            })
            .collect();
        init.into_iter()
            .rev()
            .zip(stacks.iter().cycle())
            .for_each(|(message, stack)| stack.push(message));
        stacks
    }

    fn push(&self, message: Message<W>) {
        self.deque.push(message);
    }

    fn pop(&self) -> Option<Message<W>> {
        self.deque.pop().or_else(|| self.steal())
    }

    fn steal(&self) -> Option<Message<W>> {
        let (left, right) = self.stealers.split_at(self.index);
        let right = &right[1..];
        right
            .iter()
            .chain(left.iter())
            .map(|stealer| stealer.steal_batch_and_pop(&self.deque))
            .find_map(|steal| steal.success())
    }
}

pub(crate) fn walk_latest_mtime(
    inner: Arc<Inner>,
    plan: WalkPlan,
    mode: WalkMode,
    collect_diagnostics: bool,
) -> Result<(WalkOutcome, WalkDiagnostics), VfsError> {
    match mode {
        WalkMode::ForceDisk | WalkMode::Hybrid => {
            walk_disk_populate(inner, plan, collect_diagnostics)
        }
        WalkMode::IndexOnly => walk_index_only(inner, plan, collect_diagnostics),
    }
}

fn thread_count() -> usize {
    thread::available_parallelism()
        .map_or(1, |threads| threads.get())
        .min(12)
}

struct SharedDiskWalk {
    inner: Arc<Inner>,
    plan: WalkPlan,
    quit_now: AtomicBool,
    active_workers: AtomicUsize,
    errors: Mutex<Vec<VfsError>>,
    delta: Mutex<DiskDelta>,
    read_dir_nanos: AtomicU64,
    glob_match_nanos: AtomicU64,
    dirs_visited: AtomicUsize,
    entries_seen: AtomicUsize,
    file_candidates: AtomicUsize,
    matched_files_seen: AtomicUsize,
    collect_diagnostics: bool,
}

fn walk_disk_populate(
    inner: Arc<Inner>,
    plan: WalkPlan,
    collect_diagnostics: bool,
) -> Result<(WalkOutcome, WalkDiagnostics), VfsError> {
    let total_start = collect_diagnostics.then(Instant::now);
    let disk_walk_start = collect_diagnostics.then(Instant::now);
    let threads = thread_count();
    let shared = Arc::new(SharedDiskWalk {
        inner,
        plan: plan.clone(),
        quit_now: AtomicBool::new(false),
        active_workers: AtomicUsize::new(threads),
        errors: Mutex::new(Vec::new()),
        delta: Mutex::new(DiskDelta::default()),
        read_dir_nanos: AtomicU64::new(0),
        glob_match_nanos: AtomicU64::new(0),
        dirs_visited: AtomicUsize::new(0),
        entries_seen: AtomicUsize::new(0),
        file_candidates: AtomicUsize::new(0),
        matched_files_seen: AtomicUsize::new(0),
        collect_diagnostics,
    });
    let stacks = Stack::new_for_each_thread(
        threads,
        vec![Message::Work(DiskWork {
            dir: plan.start_dir,
            depth_from_start: 0,
        })],
    );

    thread::scope(|scope| {
        let handles: Vec<_> = stacks
            .into_iter()
            .map(|stack| {
                let shared = shared.clone();
                scope.spawn(move || {
                    DiskWorker {
                        shared,
                        stack,
                        local: DiskDelta::default(),
                    }
                    .run()
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("indexed disk walker worker panicked");
        }
    });

    let disk_walk = disk_walk_start
        .map(|start| start.elapsed())
        .unwrap_or_default();

    if let Some(error) = shared.errors.lock().pop() {
        return Err(error);
    }

    let delta = std::mem::take(&mut *shared.delta.lock());
    let mut diagnostics = if collect_diagnostics {
        WalkDiagnostics {
            disk_walk,
            read_dir_cumulative: Duration::from_nanos(shared.read_dir_nanos.load(Ordering::SeqCst)),
            glob_match_cumulative: Duration::from_nanos(
                shared.glob_match_nanos.load(Ordering::SeqCst),
            ),
            dirs_visited: shared.dirs_visited.load(Ordering::SeqCst),
            entries_seen: shared.entries_seen.load(Ordering::SeqCst),
            file_candidates: shared.file_candidates.load(Ordering::SeqCst),
            matched_files_seen: shared.matched_files_seen.load(Ordering::SeqCst),
            ..WalkDiagnostics::default()
        }
    } else {
        WalkDiagnostics::default()
    };
    let outcome = commit_disk_delta(&shared.inner, delta, collect_diagnostics, &mut diagnostics);
    if let Some(total_start) = total_start {
        diagnostics.total = total_start.elapsed();
    }
    Ok((outcome, diagnostics))
}

pub(crate) fn commit_disk_delta(
    inner: &Inner,
    mut delta: DiskDelta,
    collect_diagnostics: bool,
    diagnostics: &mut WalkDiagnostics,
) -> WalkOutcome {
    let commit_start = collect_diagnostics.then(Instant::now);
    let sort_start = collect_diagnostics.then(Instant::now);
    // Directories must be inserted parent-before-child so parent links can be
    // filled while preserving stable ids. Matched files are not sorted here:
    // each directory entry is visited once, and avoiding a global file sort is
    // important for cold-populate performance.
    delta
        .visited_dirs
        .sort_by_key(|path| path.components().count());
    delta.visited_dirs.dedup();
    if let Some(sort_start) = sort_start {
        diagnostics.sort_dedup = sort_start.elapsed();
        diagnostics.dirs_indexed = delta.visited_dirs.len();
        diagnostics.files_indexed = delta.matched_files.len();
    }

    // Hold the index lock only for the batched commit. The expensive backend
    // traversal above ran without this lock.
    let mut index = inner.index.lock();
    let commit_dirs_start = collect_diagnostics.then(Instant::now);
    for dir in delta.visited_dirs {
        index.upsert_query_dir(dir);
    }
    if let Some(commit_dirs_start) = commit_dirs_start {
        diagnostics.commit_dirs = commit_dirs_start.elapsed();
    }

    let commit_files_start = collect_diagnostics.then(Instant::now);
    let mut outcome = WalkOutcome::new();
    for matched in delta.matched_files {
        let file = index.upsert_query_file(
            matched.path.clone(),
            matched.kind,
            matched.size,
            matched.modified,
        );
        outcome.matched_files.insert(file);
        if let Some(modified) = matched.modified {
            outcome
                .max_by_mtime
                .insert(MatchEntry::new(modified, matched.path, file));
        }
    }
    if let Some(commit_files_start) = commit_files_start {
        diagnostics.commit_files = commit_files_start.elapsed();
    }
    if let Some(commit_start) = commit_start {
        diagnostics.commit_total = commit_start.elapsed();
    }
    outcome
}

struct DiskWorker {
    shared: Arc<SharedDiskWalk>,
    stack: Stack<DiskWork>,
    local: DiskDelta,
}

impl DiskWorker {
    fn run(mut self) {
        while let Some(work) = self.get_work() {
            if self.shared.quit_now.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = self.run_one(work) {
                self.shared.errors.lock().push(error);
                self.shared.quit_now.store(true, Ordering::SeqCst);
                self.send_quit();
                break;
            }
        }
        // Merge once per worker to keep the hot traversal path mostly local.
        self.shared.delta.lock().merge(self.local);
    }

    fn run_one(&mut self, work: DiskWork) -> Result<(), VfsError> {
        if self.shared.collect_diagnostics {
            self.shared.dirs_visited.fetch_add(1, Ordering::Relaxed);
        }
        self.local.visited_dirs.push(work.dir.clone());
        let read_start = self.shared.collect_diagnostics.then(Instant::now);
        let entries = match self.shared.inner.read_dir_entries(&work.dir) {
            Ok(entries) => entries,
            Err(VfsError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                if let Some(read_start) = read_start {
                    add_duration(&self.shared.read_dir_nanos, read_start.elapsed());
                }
                return Ok(());
            }
            Err(error) => {
                if let Some(read_start) = read_start {
                    add_duration(&self.shared.read_dir_nanos, read_start.elapsed());
                }
                return Err(error);
            }
        };
        if let Some(read_start) = read_start {
            add_duration(&self.shared.read_dir_nanos, read_start.elapsed());
        }

        for entry in entries {
            if self.shared.collect_diagnostics {
                self.shared.entries_seen.fetch_add(1, Ordering::Relaxed);
            }
            match entry.kind {
                VfsEntryKind::Directory => {
                    if self.shared.plan.should_descend(work.depth_from_start) {
                        self.send(DiskWork {
                            dir: entry.path,
                            depth_from_start: work.depth_from_start + 1,
                        });
                    }
                }
                kind => {
                    if self.shared.collect_diagnostics {
                        self.shared.file_candidates.fetch_add(1, Ordering::Relaxed);
                    }
                    let match_start = self.shared.collect_diagnostics.then(Instant::now);
                    let matches = glob_matches(
                        &self.shared.plan.root,
                        &entry.path,
                        &self.shared.plan.pattern,
                    );
                    if let Some(match_start) = match_start {
                        add_duration(&self.shared.glob_match_nanos, match_start.elapsed());
                    }
                    if matches {
                        if self.shared.collect_diagnostics {
                            self.shared
                                .matched_files_seen
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        self.local.matched_files.push(MatchedFileDelta {
                            path: entry.path,
                            kind,
                            size: entry.size,
                            modified: entry.modified,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn get_work(&mut self) -> Option<DiskWork> {
        get_work(
            &self.stack,
            &self.shared.quit_now,
            &self.shared.active_workers,
        )
    }

    fn send(&self, work: DiskWork) {
        self.stack.push(Message::Work(work));
    }

    fn send_quit(&self) {
        self.stack.push(Message::Quit);
    }
}

struct SharedIndexWalk {
    inner: Arc<Inner>,
    plan: WalkPlan,
    quit_now: AtomicBool,
    active_workers: AtomicUsize,
    errors: Mutex<Vec<VfsError>>,
    outcome: Mutex<WalkOutcome>,
}

fn walk_index_only(
    inner: Arc<Inner>,
    plan: WalkPlan,
    collect_diagnostics: bool,
) -> Result<(WalkOutcome, WalkDiagnostics), VfsError> {
    let total_start = collect_diagnostics.then(Instant::now);
    let index_walk_start = collect_diagnostics.then(Instant::now);
    let start_dir = inner
        .index
        .lock()
        .dir_id_for_path(&plan.start_dir)
        .ok_or_else(|| VfsError::IndexMiss {
            needed: "start directory",
            path: plan.start_dir.clone(),
        })?;
    let threads = thread_count();
    let shared = Arc::new(SharedIndexWalk {
        inner,
        plan,
        quit_now: AtomicBool::new(false),
        active_workers: AtomicUsize::new(threads),
        errors: Mutex::new(Vec::new()),
        outcome: Mutex::new(WalkOutcome::new()),
    });
    let stacks = Stack::new_for_each_thread(
        threads,
        vec![Message::Work(IndexWork {
            dir: start_dir,
            depth_from_start: 0,
        })],
    );

    thread::scope(|scope| {
        let handles: Vec<_> = stacks
            .into_iter()
            .map(|stack| {
                let shared = shared.clone();
                scope.spawn(move || {
                    IndexWorker {
                        shared,
                        stack,
                        local: WalkOutcome::new(),
                    }
                    .run()
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("indexed walker worker panicked");
        }
    });

    let index_walk = index_walk_start
        .map(|start| start.elapsed())
        .unwrap_or_default();
    if let Some(error) = shared.errors.lock().pop() {
        return Err(error);
    }
    let mut outcome = shared.outcome.lock();
    Ok((
        std::mem::replace(&mut *outcome, WalkOutcome::new()),
        WalkDiagnostics {
            total: total_start.map(|start| start.elapsed()).unwrap_or_default(),
            index_walk,
            ..WalkDiagnostics::default()
        },
    ))
}

struct IndexWorker {
    shared: Arc<SharedIndexWalk>,
    stack: Stack<IndexWork>,
    local: WalkOutcome,
}

impl IndexWorker {
    fn run(mut self) {
        while let Some(work) = self.get_work() {
            if self.shared.quit_now.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = self.run_one(work) {
                self.shared.errors.lock().push(error);
                self.shared.quit_now.store(true, Ordering::SeqCst);
                self.send_quit();
                break;
            }
        }
        self.shared.outcome.lock().merge(self.local);
    }

    fn run_one(&mut self, work: IndexWork) -> Result<(), VfsError> {
        let children = self.children_for(work.dir)?;
        for node in children {
            if let Some(dir) = {
                let index = self.shared.inner.index.lock();
                node_as_dir(&index, node)
            } {
                if self.shared.plan.should_descend(work.depth_from_start) {
                    self.send(IndexWork {
                        dir,
                        depth_from_start: work.depth_from_start + 1,
                    });
                }
                continue;
            }
            if let Some(file) = {
                let index = self.shared.inner.index.lock();
                node_as_file(&index, node)
            } {
                self.visit_file(file)?;
            }
        }
        Ok(())
    }

    fn children_for(&self, dir: DirId) -> Result<Vec<crate::index::NodeId>, VfsError> {
        let index = self.shared.inner.index.lock();
        index
            .clean_dir_children(dir)
            .ok_or_else(|| VfsError::IndexMiss {
                needed: "clean directory listing",
                path: index.dir_path(dir).to_path_buf(),
            })
    }

    fn visit_file(&mut self, file: FileId) -> Result<(), VfsError> {
        let path = self.shared.inner.index.lock().file_path(file).to_path_buf();
        if !glob_matches(&self.shared.plan.root, &path, &self.shared.plan.pattern) {
            return Ok(());
        }
        let index = self.shared.inner.index.lock();
        let record = index.file(file);
        if record.dirty || record.modified.is_none() {
            return Err(VfsError::IndexMiss {
                needed: "file metadata",
                path,
            });
        }
        if record.exists {
            self.local.push_match(file, &index);
        }
        Ok(())
    }

    fn get_work(&mut self) -> Option<IndexWork> {
        get_work(
            &self.stack,
            &self.shared.quit_now,
            &self.shared.active_workers,
        )
    }

    fn send(&self, work: IndexWork) {
        self.stack.push(Message::Work(work));
    }

    fn send_quit(&self) {
        self.stack.push(Message::Quit);
    }
}

fn add_duration(counter: &AtomicU64, duration: Duration) {
    let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
    counter.fetch_add(nanos, Ordering::Relaxed);
}

fn get_work<W>(stack: &Stack<W>, quit_now: &AtomicBool, active_workers: &AtomicUsize) -> Option<W> {
    let mut value = stack.pop();
    loop {
        if quit_now.load(Ordering::SeqCst) {
            value = Some(Message::Quit);
        }
        match value {
            Some(Message::Work(work)) => return Some(work),
            Some(Message::Quit) => {
                stack.push(Message::Quit);
                return None;
            }
            None => {
                if active_workers.fetch_sub(1, Ordering::Acquire) - 1 == 0 {
                    stack.push(Message::Quit);
                    return None;
                }
                loop {
                    if let Some(message) = stack.pop() {
                        active_workers.fetch_add(1, Ordering::Release);
                        value = Some(message);
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}
