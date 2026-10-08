//! Folder-size index.
//!
//! Compared with the previous version:
//!  * the cache is shared (`Arc<RwLock<..>>`) instead of being cloned on every navigation;
//!  * scanning is iterative, runs at background thread priority and yields regularly;
//!  * every finished sub-folder is stored immediately, so cancelled scans keep their progress;
//!  * entries are only trusted while the folder's mtime matches *and* they are younger than `TTL_SECS`
//!    (a directory's mtime does not change when a file deeper down grows, so mtime alone goes stale);
//!  * a file watcher invalidates the changed path and all its ancestors, so new files show up live;
//!  * the cache is written atomically from a background thread.

use eframe::egui;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
        Arc, RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const INDEX_FILE: &str = "folder_sizes.json";
/// Maximum age before a cached folder is re-scanned even if its mtime is unchanged.
const TTL_SECS: u64 = 6 * 3600;
/// Entries not refreshed for this long are dropped when the cache is saved.
const PRUNE_SECS: u64 = 30 * 24 * 3600;

#[derive(Clone, Serialize, Deserialize)]
pub struct CachedFolder {
    pub size: u64,
    pub modified_ns: u128,
    /// Unix seconds of the scan. Missing in old cache files (0 -> treated as expired once).
    #[serde(default)]
    pub scanned_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct IndexFile {
    folder_sizes: HashMap<String, CachedFolder>,
}

pub enum IndexMsg {
    Size(PathBuf, u64),
}

type Cache = Arc<RwLock<HashMap<PathBuf, CachedFolder>>>;

pub struct Indexer {
    cache: Cache,
    generation: Arc<AtomicU64>,
    cache_dirty: Arc<AtomicBool>,
    tx: Sender<IndexMsg>,
    ctx: egui::Context,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn ns(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

impl Indexer {
    pub fn new(ctx: egui::Context, tx: Sender<IndexMsg>, dir: &Path) -> Self {
        let me = Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
            cache_dirty: Arc::new(AtomicBool::new(false)),
            tx,
            ctx,
        };
        me.load(dir);
        me
    }

    pub fn len(&self) -> usize {
        self.cache.read().map(|c| c.len()).unwrap_or(0)
    }

    /// Cached size regardless of age (shown immediately; fresh value follows).
    pub fn lookup(&self, path: &Path) -> Option<u64> {
        self.cache.read().ok()?.get(path).map(|c| c.size)
    }

    /// Drop the path and every ancestor: their totals are no longer correct.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut c) = self.cache.write() {
            let mut p = Some(path);
            while let Some(cur) = p {
                c.remove(cur);
                p = cur.parent();
            }
            self.cache_dirty.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.cache_dirty.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        if let Ok(mut c) = self.cache.write() {
            c.clear();
        }
        self.cache_dirty.store(true, Ordering::Relaxed);
    }

    /// Cancels any running scan and starts a new one for `folders`. Up-to-date folders return instantly.
    pub fn start(&self, folders: Vec<PathBuf>) {
        let my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if folders.is_empty() {
            return;
        }
        let cache = Arc::clone(&self.cache);
        let generation = Arc::clone(&self.generation);
        let dirty = Arc::clone(&self.cache_dirty);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            lower_priority();
            for folder in folders {
                if generation.load(Ordering::Relaxed) != my_gen {
                    return;
                }
                let stamp = fs::metadata(&folder).and_then(|m| m.modified()).map(ns).unwrap_or(0);
                let now = now_secs();
                let fresh = cache.read().ok().and_then(|c| c.get(&folder).cloned()).filter(|c| is_fresh(c, stamp, now));
                let size = match fresh {
                    Some(c) => c.size,
                    None => match scan(&folder, stamp, &cache, &generation, my_gen, &dirty) {
                        Some(s) => s,
                        None => return, // cancelled
                    },
                };
                if tx.send(IndexMsg::Size(folder, size)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
    }

    // ---------------------------------------------------------------- persistence

    pub fn load(&self, dir: &Path) {
        let Ok(text) = fs::read_to_string(dir.join(INDEX_FILE)) else { return };
        let Ok(file) = serde_json::from_str::<IndexFile>(&text) else { return };
        if let Ok(mut c) = self.cache.write() {
            *c = file.folder_sizes.into_iter().map(|(k, v)| (PathBuf::from(k), v)).collect();
        }
    }

    /// Saves in a background thread (write to temp file, then rename) if anything changed.
    pub fn save_if_dirty(&self, dir: &Path, wait: bool) {
        if !self.cache_dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let snapshot: HashMap<String, CachedFolder> = match self.cache.read() {
            Ok(c) => {
                let cutoff = now_secs().saturating_sub(PRUNE_SECS);
                c.iter()
                    .filter(|(_, v)| v.scanned_at >= cutoff)
                    .map(|(k, v)| (k.to_string_lossy().into_owned(), v.clone()))
                    .collect()
            }
            Err(_) => return,
        };
        let dir = dir.to_path_buf();
        let job = move || {
            if fs::create_dir_all(&dir).is_err() {
                return;
            }
            if let Ok(text) = serde_json::to_string(&IndexFile { folder_sizes: snapshot }) {
                let tmp = dir.join(format!("{INDEX_FILE}.tmp"));
                if fs::write(&tmp, text).is_ok() {
                    let _ = fs::rename(&tmp, dir.join(INDEX_FILE));
                }
            }
        };
        if wait {
            job();
        } else {
            std::thread::spawn(job);
        }
    }
}

fn is_fresh(c: &CachedFolder, stamp: u128, now: u64) -> bool {
    c.modified_ns == stamp && now.saturating_sub(c.scanned_at) < TTL_SECS
}

struct Frame {
    path: PathBuf,
    rd: fs::ReadDir,
    total: u64,
    stamp: u128,
}

enum Step {
    Add(u64),
    Descend(PathBuf, u128),
    Skip,
    End,
}

/// Iterative post-order walk. Returns None if cancelled.
fn scan(root: &Path, root_stamp: u128, cache: &Cache, generation: &AtomicU64, my_gen: u64, dirty: &AtomicBool) -> Option<u64> {
    let now = now_secs();
    let mut pending: Vec<(PathBuf, CachedFolder)> = Vec::new();
    let flush = |pending: &mut Vec<(PathBuf, CachedFolder)>| {
        if pending.is_empty() {
            return;
        }
        if let Ok(mut c) = cache.write() {
            c.extend(pending.drain(..));
            dirty.store(true, Ordering::Relaxed);
        }
    };

    let Ok(rd) = fs::read_dir(root) else { return Some(0) };
    let mut stack = vec![Frame { path: root.to_path_buf(), rd, total: 0, stamp: root_stamp }];
    let mut counter = 0u32;

    loop {
        if generation.load(Ordering::Relaxed) != my_gen {
            flush(&mut pending); // keep the completed sub-folders
            return None;
        }
        counter += 1;
        if counter % 4096 == 0 {
            flush(&mut pending);
            std::thread::sleep(Duration::from_millis(2)); // duty-cycle cap: leave CPU/disk for the UI
        }

        let step = {
            let top = stack.last_mut()?;
            match top.rd.next() {
                None => Step::End,
                Some(Err(_)) => Step::Skip,
                Some(Ok(de)) => match de.file_type() {
                    Ok(ft) if ft.is_symlink() => Step::Skip, // symlinks & junctions: avoid loops/double counting
                    Ok(ft) if ft.is_file() => Step::Add(de.metadata().map(|m| m.len()).unwrap_or(0)),
                    Ok(ft) if ft.is_dir() => {
                        let stamp = de.metadata().and_then(|m| m.modified()).map(ns).unwrap_or(0);
                        Step::Descend(de.path(), stamp)
                    }
                    _ => Step::Skip,
                },
            }
        };

        match step {
            Step::Skip => {}
            Step::Add(n) => {
                if let Some(top) = stack.last_mut() {
                    top.total = top.total.saturating_add(n);
                }
            }
            Step::Descend(path, stamp) => {
                let hit = cache.read().ok().and_then(|c| c.get(&path).cloned()).filter(|c| is_fresh(c, stamp, now));
                if let Some(hit) = hit {
                    if let Some(top) = stack.last_mut() {
                        top.total = top.total.saturating_add(hit.size);
                    }
                } else if let Ok(rd) = fs::read_dir(&path) {
                    stack.push(Frame { path, rd, total: 0, stamp });
                }
            }
            Step::End => {
                let done = stack.pop()?;
                pending.push((done.path, CachedFolder { size: done.total, modified_ns: done.stamp, scanned_at: now }));
                match stack.last_mut() {
                    Some(parent) => parent.total = parent.total.saturating_add(done.total),
                    None => {
                        flush(&mut pending);
                        return Some(done.total);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn lower_priority() {
    // THREAD_MODE_BACKGROUND_BEGIN lowers CPU *and* I/O priority of this thread.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(h: isize, p: i32) -> i32;
    }
    unsafe {
        SetThreadPriority(GetCurrentThread(), 0x0001_0000);
    }
}
#[cfg(not(windows))]
fn lower_priority() {}

// ----------------------------------------------------------------------------------------------
// File watcher: live refresh + cache invalidation

pub struct DirWatcher {
    watcher: RecommendedWatcher,
    current: Option<PathBuf>,
}

impl DirWatcher {
    pub fn new(ctx: egui::Context, tx: Sender<Vec<PathBuf>>) -> Option<Self> {
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(ev.kind, EventKind::Access(_)) || ev.paths.is_empty() {
                    return;
                }
                let _ = tx.send(ev.paths);
                ctx.request_repaint_after(Duration::from_millis(300));
            }
        })
        .ok()?;
        Some(Self { watcher, current: None })
    }

    pub fn watch(&mut self, dir: &Path) {
        if self.current.as_deref() == Some(dir) {
            return;
        }
        if let Some(old) = self.current.take() {
            let _ = self.watcher.unwatch(&old);
        }
        // Drive roots see constant system churn; only watch their direct children.
        let mode = if dir.parent().is_none() { RecursiveMode::NonRecursive } else { RecursiveMode::Recursive };
        if self.watcher.watch(dir, mode).is_ok() {
            self.current = Some(dir.to_path_buf());
        }
    }
}
