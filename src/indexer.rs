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
//!
//! File-name index (search):
//!  * `FileIndex` keeps, per folder, one compact blob with the names of its direct children.
//!    Opening a folder records that single level (cheap, one `read_dir`).
//!  * Searching walks the tree breadth-first *through the index*: folders already known are
//!    answered from memory, unknown or stale ones are read from disk once and stored, so the
//!    first deep search of a tree indexes it and later searches are instant.
//!  * Folders are read by a few parallel readers at slightly-below-normal priority, the walk is
//!    cancellable within one folder and never follows symlinks/junctions; memory is capped (`MAX_INDEX_BYTES`).
//!  * The index is saved to `file_names.idx` (background thread, at most once a minute, never
//!    while closing) and loaded in the background at start-up. Loaded folders are re-validated
//!    by their modification time before being trusted.

use eframe::egui;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::Sender,
        Arc, Mutex, RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const INDEX_FILE: &str = "folder_sizes.json";
/// Maximum age before a cached folder is re-scanned even if its mtime is unchanged.
const TTL_SECS: u64 = 6 * 3600;
/// Entries not refreshed for this long are dropped when the cache is saved.
const PRUNE_SECS: u64 = 30 * 24 * 3600;
/// Only the top levels of a scan and folders at least this big are cached. Caching every folder
/// of a whole drive meant millions of entries: gigabytes of RAM and a cache file that took
/// seconds to write when closing the app.
const KEEP_BYTES: u64 = 64 << 20;
const KEEP_DEPTH: usize = 4;
/// A folder listing younger than this is trusted without even a `stat`.
const FRESH_SECS: u64 = 300;
/// Memory budget of the file-name index (names are stored back to back, ~1 byte per name char).
const MAX_INDEX_BYTES: usize = 160 << 20;
/// On-disk copy of the file-name index (binary, next to the folder-size cache).
const FILES_FILE: &str = "file_names.idx";
const FILES_MAGIC: &[u8; 8] = b"SXFNIDX1";
/// The index file is rewritten at most this often (it can be tens of MB).
const FILES_SAVE_GAP_SECS: u64 = 60;
/// Parallel directory readers during a search. Reading a folder is a chain of syscalls (open,
/// query, close, plus antivirus filters), so more readers than cores still help: while one waits
/// in the kernel another runs. This is deliberately aggressive (like Explorer's search); lower
/// `SEARCH_THREADS_MAX` or the multiplier to trade speed for CPU.
const SEARCH_THREADS_MAX: usize = 24;
fn search_threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (cores * 3 / 2).clamp(6, SEARCH_THREADS_MAX)
}

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
    files: FileIndex,
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
            files: FileIndex::new(),
        };
        me.load(dir);
        me
    }

    pub fn len(&self) -> usize {
        self.cache.read().map(|c| c.len()).unwrap_or(0)
    }

    /// Handle to the file-name index (cheap to clone, shareable with worker threads).
    pub fn files(&self) -> FileIndex {
        self.files.clone()
    }

    /// Cached size regardless of age (shown immediately; fresh value follows).
    pub fn lookup(&self, path: &Path) -> Option<u64> {
        self.cache.read().ok()?.get(path).map(|c| c.size)
    }

    /// Drop the path and every ancestor: their totals are no longer correct.
    pub fn invalidate(&self, path: &Path) {
        self.files.invalidate(path);
        if let Ok(mut c) = self.cache.write() {
            let mut p = Some(path);
            while let Some(cur) = p {
                c.remove(cur);
                p = cur.parent();
            }
            self.cache_dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Forgets everything: stops a running scan, empties the cache and removes the saved file.
    pub fn delete_index(&self, dir: &Path) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.files.clear();
        if let Ok(mut c) = self.cache.write() {
            c.clear();
        }
        self.cache_dirty.store(false, Ordering::Relaxed);
        let _ = fs::remove_file(dir.join(INDEX_FILE));
        let _ = fs::remove_file(dir.join(format!("{INDEX_FILE}.tmp")));
        let _ = fs::remove_file(dir.join(FILES_FILE));
        let _ = fs::remove_file(dir.join(format!("{FILES_FILE}.tmp")));
        self.files.dirty.store(false, Ordering::Relaxed);
    }

    pub fn is_dirty(&self) -> bool {
        self.cache_dirty.load(Ordering::Relaxed) || self.files.is_dirty()
    }

    pub fn clear(&self) {
        self.files.clear();
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
        self.files.load_async(dir);
        let Ok(text) = fs::read_to_string(dir.join(INDEX_FILE)) else { return };
        let Ok(file) = serde_json::from_str::<IndexFile>(&text) else { return };
        if let Ok(mut c) = self.cache.write() {
            *c = file.folder_sizes.into_iter().map(|(k, v)| (PathBuf::from(k), v)).collect();
        }
    }

    /// Saves in a background thread (write to temp file, then rename) if anything changed.
    pub fn save_if_dirty(&self, dir: &Path, wait: bool) {
        if !wait {
            // Never on the closing path: the name index can be big and closing must stay instant.
            self.files.save(dir);
        }
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
                let depth = stack.len(); // counts the folder being finished
                let done = stack.pop()?;
                if depth <= KEEP_DEPTH || done.total >= KEEP_BYTES {
                    pending.push((done.path, CachedFolder { size: done.total, modified_ns: done.stamp, scanned_at: now }));
                }
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
pub fn lower_priority() {
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
pub fn lower_priority() {}

/// Interactive work (a search the user is waiting for): slightly below the UI thread, but with
/// normal I/O priority. (`lower_priority`'s background mode also drops I/O to "very low", which
/// makes directory reads crawl and leaves the CPU almost idle.)
#[cfg(windows)]
pub fn below_normal_priority() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(h: isize, p: i32) -> i32;
    }
    unsafe {
        SetThreadPriority(GetCurrentThread(), -1); // THREAD_PRIORITY_BELOW_NORMAL
    }
}
#[cfg(not(windows))]
pub fn below_normal_priority() {}

// ----------------------------------------------------------------------------------------------
// File-name index: recursive search without hammering the disk

const F_DIR: u8 = 1;
const F_HIDDEN: u8 = 2;
const F_LINK: u8 = 4;

/// The direct children of one folder. `names` holds `<flag char><name>/` per child (`/` can't
/// occur in a file name), so a million files cost roughly their name lengths in memory.
struct DirRec {
    stamp: u128,
    scanned_at: AtomicU64,
    names: Box<str>,
}

fn rec_cost(dir: &Path, rec: &DirRec) -> usize {
    rec.names.len() + dir.as_os_str().len() + 96
}

fn read_rec(dir: &Path) -> Option<DirRec> {
    // Stamp first: if the folder changes while we read, the next check simply rescans.
    let stamp = fs::metadata(dir).and_then(|m| m.modified()).map(ns).unwrap_or(0);
    let rd = fs::read_dir(dir).ok()?;
    let mut names = String::new();
    for item in rd.flatten() {
        let Ok(ft) = item.file_type() else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if name.contains('/') {
            continue;
        }
        let hidden = match item.metadata() {
            Ok(md) => crate::fsops::is_hidden_entry(&name, &md),
            Err(_) => name.starts_with('.'),
        };
        let mut flags = 0u8;
        if ft.is_dir() {
            flags |= F_DIR;
        }
        if hidden {
            flags |= F_HIDDEN;
        }
        if ft.is_symlink() {
            flags |= F_LINK; // symlinks & junctions are listed but never entered
        }
        names.push((b'0' + flags) as char);
        names.push_str(&name);
        names.push('/');
    }
    names.shrink_to_fit();
    Some(DirRec { stamp, scanned_at: AtomicU64::new(now_secs()), names: names.into_boxed_str() })
}

/// `(flags, name)` of every child.
fn children(rec: &DirRec) -> impl Iterator<Item = (u8, &str)> {
    rec.names.split('/').filter_map(|item| {
        let b = *item.as_bytes().first()?;
        Some((b.wrapping_sub(b'0'), &item[1..]))
    })
}

/// Case-insensitive substring test; `q` must already be lower-case.
fn name_matches(name: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    if name.is_ascii() && q.is_ascii() {
        let (n, qb) = (name.as_bytes(), q.as_bytes());
        return qb.len() <= n.len() && n.windows(qb.len()).any(|w| w.eq_ignore_ascii_case(qb));
    }
    name.to_lowercase().contains(q)
}

fn read_record(r: &mut impl std::io::Read) -> Option<(PathBuf, Arc<DirRec>)> {
    fn u32_of(r: &mut impl std::io::Read) -> Option<u32> {
        let mut b = [0u8; 4];
        r.read_exact(&mut b).ok()?;
        Some(u32::from_le_bytes(b))
    }
    let plen = u32_of(r)? as usize;
    if plen == 0 || plen > 32 * 1024 {
        return None;
    }
    let mut pb = vec![0u8; plen];
    r.read_exact(&mut pb).ok()?;
    let path = PathBuf::from(String::from_utf8(pb).ok()?);
    let mut sb = [0u8; 16];
    r.read_exact(&mut sb).ok()?;
    let mut tb = [0u8; 8];
    r.read_exact(&mut tb).ok()?;
    let nlen = u32_of(r)? as usize;
    if nlen > 256 << 20 {
        return None;
    }
    let mut nb = vec![0u8; nlen];
    r.read_exact(&mut nb).ok()?;
    let names = String::from_utf8(nb).ok()?.into_boxed_str();
    Some((
        path,
        Arc::new(DirRec {
            stamp: u128::from_le_bytes(sb),
            scanned_at: AtomicU64::new(u64::from_le_bytes(tb)),
            names,
        }),
    ))
}

pub struct Hit {
    pub path: PathBuf,
    pub is_dir: bool,
}

pub enum SearchEnd {
    /// Every reachable folder was visited.
    Done,
    /// The caller's `cancelled` returned true.
    Cancelled,
    /// `on_hit` asked to stop (result limit reached).
    Stopped,
}

#[derive(Clone)]
pub struct FileIndex {
    dirs: Arc<RwLock<HashMap<PathBuf, Arc<DirRec>>>>,
    bytes: Arc<AtomicUsize>,
    dirty: Arc<AtomicBool>,
    saving: Arc<AtomicBool>,
    last_save: Arc<AtomicU64>,
}

impl FileIndex {
    fn new() -> Self {
        Self {
            dirs: Arc::new(RwLock::new(HashMap::new())),
            bytes: Arc::new(AtomicUsize::new(0)),
            dirty: Arc::new(AtomicBool::new(false)),
            saving: Arc::new(AtomicBool::new(false)),
            last_save: Arc::new(AtomicU64::new(now_secs())),
        }
    }

    pub fn clear(&self) {
        if let Ok(mut m) = self.dirs.write() {
            m.clear();
        }
        self.bytes.store(0, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Something in `path` changed: its own listing and its parent's listing are stale.
    /// (Deeper levels stay valid; vanished sub-folders are pruned when the parent is re-read.)
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut m) = self.dirs.write() {
            for p in [Some(path), path.parent()].into_iter().flatten() {
                if let Some(old) = m.remove(p) {
                    self.bytes.fetch_sub(rec_cost(p, &old), Ordering::Relaxed);
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    /// Called when a folder is opened: indexes that one level.
    pub fn record_shallow(&self, dir: &Path) {
        if let Some(rec) = read_rec(dir) {
            self.store(dir.to_path_buf(), Arc::new(rec));
        }
    }

    // ------------------------------------------------------------ persistence

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Reads the saved index in a background thread. Entries found live in the meantime win,
    /// so a search that starts before loading has finished is never overwritten by old data.
    pub fn load_async(&self, dir: &Path) {
        let me = self.clone();
        let path = dir.join(FILES_FILE);
        std::thread::spawn(move || {
            lower_priority();
            me.load_from(&path);
        });
    }

    fn load_from(&self, path: &Path) {
        use std::io::Read;
        let Ok(f) = fs::File::open(path) else { return };
        let mut r = std::io::BufReader::with_capacity(1 << 20, f);
        let mut magic = [0u8; 8];
        if r.read_exact(&mut magic).is_err() || &magic != FILES_MAGIC {
            return;
        }
        let mut batch: Vec<(PathBuf, Arc<DirRec>)> = Vec::new();
        while let Some(item) = read_record(&mut r) {
            batch.push(item);
            if batch.len() >= 512 {
                self.merge(&mut batch);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.merge(&mut batch);
    }

    fn merge(&self, batch: &mut Vec<(PathBuf, Arc<DirRec>)>) {
        let Ok(mut m) = self.dirs.write() else {
            batch.clear();
            return;
        };
        for (p, r) in batch.drain(..) {
            let cost = rec_cost(&p, &r);
            if m.contains_key(&p) || self.bytes.load(Ordering::Relaxed) + cost > MAX_INDEX_BYTES {
                continue;
            }
            self.bytes.fetch_add(cost, Ordering::Relaxed);
            m.insert(p, r);
        }
    }

    /// Writes the index (temp file, then rename) from a background thread when something changed,
    /// at most once per `FILES_SAVE_GAP_SECS` and never two writers at once.
    pub fn save(&self, dir: &Path) {
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        let now = now_secs();
        if now.saturating_sub(self.last_save.load(Ordering::Relaxed)) < FILES_SAVE_GAP_SECS {
            return;
        }
        if self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        self.last_save.store(now, Ordering::Relaxed);
        let me = self.clone();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            lower_priority();
            me.dirty.store(false, Ordering::Relaxed); // changes from here on mark it dirty again
            let ok = me.write_to(&dir).is_some();
            if !ok {
                me.dirty.store(true, Ordering::Relaxed);
            }
            me.saving.store(false, Ordering::SeqCst);
        });
    }

    fn write_to(&self, dir: &Path) -> Option<()> {
        use std::io::Write;
        let cutoff = now_secs().saturating_sub(PRUNE_SECS);
        // Snapshot of Arcs only: the lock is held for a moment, the writing happens outside it.
        let snapshot: Vec<(PathBuf, Arc<DirRec>)> = self
            .dirs
            .read()
            .ok()?
            .iter()
            .filter(|(_, r)| r.scanned_at.load(Ordering::Relaxed) >= cutoff)
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();
        fs::create_dir_all(dir).ok()?;
        let tmp = dir.join(format!("{FILES_FILE}.tmp"));
        let mut w = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(&tmp).ok()?);
        w.write_all(FILES_MAGIC).ok()?;
        for (i, (path, rec)) in snapshot.iter().enumerate() {
            let Some(p) = path.to_str() else { continue };
            w.write_all(&(p.len() as u32).to_le_bytes()).ok()?;
            w.write_all(p.as_bytes()).ok()?;
            w.write_all(&rec.stamp.to_le_bytes()).ok()?;
            w.write_all(&rec.scanned_at.load(Ordering::Relaxed).to_le_bytes()).ok()?;
            w.write_all(&(rec.names.len() as u32).to_le_bytes()).ok()?;
            w.write_all(rec.names.as_bytes()).ok()?;
            if i % 2048 == 2047 {
                std::thread::sleep(Duration::from_millis(1)); // keep the disk free for the UI
            }
        }
        w.flush().ok()?;
        drop(w);
        fs::rename(&tmp, dir.join(FILES_FILE)).ok()
    }

    fn store(&self, dir: PathBuf, rec: Arc<DirRec>) {
        let mut vanished: Vec<PathBuf> = Vec::new();
        {
            let Ok(mut m) = self.dirs.write() else { return };
            if let Some(old) = m.get(&dir) {
                // sub-folders that existed before but are gone now: their records are garbage
                let now: HashSet<&str> =
                    children(&rec).filter(|(f, _)| f & F_DIR != 0).map(|(_, n)| n).collect();
                for (f, n) in children(old) {
                    if f & F_DIR != 0 && f & F_LINK == 0 && !now.contains(n) {
                        vanished.push(dir.join(n));
                    }
                }
                self.bytes.fetch_sub(rec_cost(&dir, old), Ordering::Relaxed);
            } else if self.bytes.load(Ordering::Relaxed) + rec_cost(&dir, &rec) > MAX_INDEX_BYTES {
                return; // budget exhausted: searching still works, it just isn't remembered
            }
            self.bytes.fetch_add(rec_cost(&dir, &rec), Ordering::Relaxed);
            m.insert(dir, rec);
            self.dirty.store(true, Ordering::Relaxed);
        }
        for v in vanished {
            self.forget_subtree(&v);
        }
    }

    fn forget_subtree(&self, root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            let old = match self.dirs.write() {
                Ok(mut m) => m.remove(&p),
                Err(_) => return,
            };
            if let Some(old) = old {
                self.dirty.store(true, Ordering::Relaxed);
                self.bytes.fetch_sub(rec_cost(&p, &old), Ordering::Relaxed);
                for (f, n) in children(&old) {
                    if f & F_DIR != 0 && f & F_LINK == 0 {
                        stack.push(p.join(n));
                    }
                }
            }
        }
    }

    /// The stored listing if it can still be trusted: young, or the folder's mtime is unchanged
    /// (a folder's mtime changes whenever a direct child is added, removed or renamed).
    fn get_fresh(&self, dir: &Path) -> Option<Arc<DirRec>> {
        let rec = self.dirs.read().ok()?.get(dir).cloned()?;
        let now = now_secs();
        if now.saturating_sub(rec.scanned_at.load(Ordering::Relaxed)) < FRESH_SECS {
            return Some(rec);
        }
        let stamp = fs::metadata(dir).and_then(|m| m.modified()).map(ns).ok()?;
        if stamp == rec.stamp {
            rec.scanned_at.store(now, Ordering::Relaxed);
            Some(rec)
        } else {
            None
        }
    }

    /// Reads one folder (from the index or from disk, remembering it), sends matching children
    /// to `tx` and collects the sub-folders still to visit.
    fn visit(
        &self,
        dir: &Path,
        root: &Path,
        q: &str,
        show_hidden: bool,
        tx: &std::sync::mpsc::Sender<Hit>,
        subdirs: &mut Vec<PathBuf>,
    ) {
        let rec = match self.get_fresh(dir) {
            Some(r) => r,
            None => match read_rec(dir) {
                Some(r) => {
                    let r = Arc::new(r);
                    self.store(dir.to_path_buf(), Arc::clone(&r));
                    r
                }
                None => {
                    if dir != root {
                        self.forget_subtree(dir); // deleted or no access
                    }
                    return;
                }
            },
        };
        for (flags, name) in children(&rec) {
            if flags & F_HIDDEN != 0 && !show_hidden {
                continue;
            }
            let is_dir = flags & F_DIR != 0;
            if name_matches(name, q) && tx.send(Hit { path: dir.join(name), is_dir }).is_err() {
                return; // the search was stopped
            }
            if is_dir && flags & F_LINK == 0 {
                subdirs.push(dir.join(name));
            }
        }
    }

    /// Search below `root`, nearest folders first (roughly: several readers work through a
    /// shared first-in-first-out queue). Known folders come from memory; unknown/stale ones are
    /// read from disk by a few parallel readers and remembered. `query` is a case-insensitive
    /// substring of the name. Hidden items are skipped unless `show_hidden`.
    /// `cancelled` is polled ~30 times a second. `on_event` runs on the calling thread: with
    /// `Some(hit)` for every match, and with `None` ~30 times a second while nothing arrives
    /// (so the caller can push partial results out). Returning false stops the search.
    pub fn search(
        &self,
        root: &Path,
        query: &str,
        show_hidden: bool,
        cancelled: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(Option<Hit>) -> bool,
    ) -> SearchEnd {
        use std::sync::{mpsc, Condvar};
        let q = query.trim().to_lowercase();
        let queue: Mutex<VecDeque<PathBuf>> = Mutex::new(VecDeque::from([root.to_path_buf()]));
        let wake = Condvar::new();
        let pending = AtomicUsize::new(1); // folders queued or being read
        let waiting = AtomicUsize::new(0); // readers currently idle, waiting for work
        let stop = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<Hit>();

        std::thread::scope(|s| {
            for _ in 0..search_threads() {
                let tx = tx.clone();
                let (queue, wake, pending, waiting, stop, q) =
                    (&queue, &wake, &pending, &waiting, &stop, &q);
                s.spawn(move || {
                    below_normal_priority();
                    loop {
                        let dir = {
                            let mut g = queue.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                if let Some(d) = g.pop_front() {
                                    break d;
                                }
                                if pending.load(Ordering::SeqCst) == 0 {
                                    return; // nothing queued, nothing in flight: finished
                                }
                                waiting.fetch_add(1, Ordering::SeqCst);
                                g = wake
                                    .wait_timeout(g, Duration::from_millis(20))
                                    .unwrap_or_else(|e| e.into_inner())
                                    .0;
                                waiting.fetch_sub(1, Ordering::SeqCst);
                            }
                        };
                        let mut subdirs = Vec::new();
                        self.visit(&dir, root, q, show_hidden, &tx, &mut subdirs);
                        let n_new = subdirs.len();
                        if !subdirs.is_empty() {
                            pending.fetch_add(subdirs.len(), Ordering::SeqCst);
                            queue.lock().unwrap_or_else(|e| e.into_inner()).extend(subdirs);
                        }
                        let left = pending.fetch_sub(1, Ordering::SeqCst) - 1;
                        // Wake idle readers only when it matters (new work, or all done); waking
                        // everyone after every folder made the readers fight over the queue.
                        if waiting.load(Ordering::SeqCst) > 0 && (n_new > 0 || left == 0) {
                            wake.notify_all();
                        }
                    }
                });
            }
            drop(tx); // the readers hold the only senders: when they all end, recv disconnects

            let mut end = SearchEnd::Done;
            loop {
                if cancelled() {
                    end = SearchEnd::Cancelled;
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(30)) {
                    Ok(hit) => {
                        if !on_event(Some(hit)) {
                            end = SearchEnd::Stopped;
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if !on_event(None) {
                            end = SearchEnd::Stopped;
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            stop.store(true, Ordering::Relaxed);
            wake.notify_all();
            end
        })
    }
}

// ----------------------------------------------------------------------------------------------
// File watcher: live refresh + cache invalidation

/// Most distinct changed paths remembered between two UI frames; more than that collapses into
/// a single "something changed" marker (the empty path).
const MAX_PENDING: usize = 2048;

pub struct DirWatcher {
    watcher: RecommendedWatcher,
    current: Option<PathBuf>,
    pending: Arc<Mutex<HashSet<PathBuf>>>,
}

impl DirWatcher {
    pub fn new(ctx: egui::Context) -> Option<Self> {
        // Events go into a bounded set that the UI drains once per frame. (An unbounded channel
        // grows without limit while the window is minimised or hidden and nobody reads it.)
        let pending: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let sink = Arc::clone(&pending);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(ev.kind, EventKind::Access(_)) || ev.paths.is_empty() {
                    return;
                }
                if let Ok(mut set) = sink.lock() {
                    for p in ev.paths {
                        if set.len() < MAX_PENDING {
                            set.insert(p);
                        } else {
                            set.insert(PathBuf::new());
                            break;
                        }
                    }
                }
                ctx.request_repaint_after(Duration::from_millis(300));
            }
        })
        .ok()?;
        Some(Self { watcher, current: None, pending })
    }

    /// Changed paths since the last call. An empty path means "too many to list".
    pub fn take(&self) -> Vec<PathBuf> {
        match self.pending.lock() {
            Ok(mut s) => std::mem::take(&mut *s).into_iter().collect(),
            Err(_) => Vec::new(),
        }
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
