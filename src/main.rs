#![cfg_attr(windows, windows_subsystem = "windows")]

mod fsops;
mod icons;
mod indexer;
mod peers;
mod theme;
mod trashbin;

use eframe::{
    egui::{
        self, pos2, vec2, Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Margin, Rect,
        RichText, Sense, Stroke, StrokeKind, Ui,
    }
};
use fsops::*;
use icons::*;
use indexer::*;
use peers::{cursor_screen_pos, now_ms, Peers, WinInfo};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque},
    fs,
    hash::{Hash, Hasher},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering as AO},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use sysinfo::Disks;
use theme::*;

// ==============================================================================================
// Data

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    name_lc: String,
    is_dir: bool,
    is_link: bool,
    size: u64,
    modified: Option<SystemTime>,
    date_text: String,
    type_text: String,
    icon: &'static str,
    dir_size: Option<u64>,
}

impl Entry {
    fn sort_size(&self) -> u64 {
        if self.is_dir {
            self.dir_size.unwrap_or(0)
        } else {
            self.size
        }
    }
}

#[derive(Clone)]
struct DriveInfo {
    mount_point: PathBuf,
    name: String,
    total: u64,
    available: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Name,
    Modified,
    Type,
    Size,
}

struct RenameState {
    path: PathBuf,
    text: String,
    focus_pending: bool,
    is_dir: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrashSort {
    Name,
    Location,
    Deleted,
    Size,
}

struct TrashState {
    rows: Vec<trashbin::TrashRow>,
    view: Vec<usize>,
    selected: HashSet<usize>,
    anchor: Option<usize>,
    loading: bool,
    seq: u64,
    error: Option<String>,
    sort_col: TrashSort,
    sort_asc: bool,
    needs_sort: bool,
    scroll_to: Option<usize>,
}

impl TrashState {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            view: Vec::new(),
            selected: HashSet::new(),
            anchor: None,
            loading: true,
            seq: 0,
            error: None,
            sort_col: TrashSort::Deleted,
            sort_asc: false,
            needs_sort: false,
            scroll_to: None,
        }
    }
}

struct ConfirmPurge {
    items: Vec<trashbin::Item>,
    text: String,
}

struct Tab {
    id: u64,
    dir: PathBuf,
    back: Vec<PathBuf>,
    fwd: Vec<PathBuf>,
    entries: Vec<Entry>,
    lookup: HashMap<PathBuf, usize>,
    view: Vec<usize>,
    view_dirty: bool,
    search: String,
    selected: HashSet<PathBuf>,
    /// Last clicked row; start of shift-ranges and origin of keyboard navigation.
    anchor: Option<PathBuf>,
    error: Option<String>,
    loading: bool,
    load_gen: u64,
    sort_col: SortColumn,
    sort_asc: bool,
    address: String,
    editing_addr: bool,
    addr_focus: bool,
    scroll_to: Option<usize>,
    rename: Option<RenameState>,
    pending_rename: Option<PathBuf>,
    trash: Option<TrashState>,
}

impl Tab {
    fn new(id: u64, dir: PathBuf) -> Self {
        Self {
            id,
            address: dir.display().to_string(),
            dir,
            back: Vec::new(),
            fwd: Vec::new(),
            entries: Vec::new(),
            lookup: HashMap::new(),
            view: Vec::new(),
            view_dirty: true,
            search: String::new(),
            selected: HashSet::new(),
            anchor: None,
            error: None,
            loading: false,
            load_gen: 0,
            sort_col: SortColumn::Name,
            sort_asc: true,
            editing_addr: false,
            addr_focus: false,
            scroll_to: None,
            rename: None,
            pending_rename: None,
            trash: None,
        }
    }

    fn select_only(&mut self, p: PathBuf) {
        self.selected.clear();
        self.selected.insert(p.clone());
        self.anchor = Some(p);
    }

    fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Selected items that are currently visible, in on-screen order.
    fn selected_paths(&self) -> Vec<PathBuf> {
        // `get`, not indexing: right after a listing refresh `view` may briefly be stale.
        self.view
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| &e.path)
            .filter(|p| self.selected.contains(*p))
            .cloned()
            .collect()
    }

    fn title(&self) -> String {
        if self.trash.is_some() {
            return "Recycle Bin".to_owned();
        }
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.dir.display().to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    font_size: f32,
    follow_text_scale: bool,
    use_system_accent: bool,
    custom_accent: [u8; 3],
    theme: ThemeChoice,
    show_hidden: bool,
    date_format: DateFormat,
    index_dir: Option<PathBuf>,
    /// Widths (at 14 pt text) of Date modified, Type, Size.
    col_widths: [f32; 3],
    /// Inner size of the window when it was last closed (points), and whether it was maximised.
    window_size: Option<[f32; 2]>,
    maximized: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            follow_text_scale: true,
            use_system_accent: true,
            custom_accent: [0x00, 0x78, 0xD4],
            theme: ThemeChoice::System,
            show_hidden: false,
            date_format: DateFormat::default(),
            index_dir: None,
            col_widths: [150.0, 110.0, 90.0],
            window_size: None,
            maximized: false,
        }
    }
}

fn app_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustExplorer")
}

fn load_settings() -> Settings {
    fs::read_to_string(app_dir().join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_settings(s: &Settings) {
    let dir = app_dir();
    let _ = fs::create_dir_all(&dir);
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = fs::write(dir.join("settings.json"), t);
    }
}

/// One decided paste operation.
struct PasteItem {
    src: PathBuf,
    dst: PathBuf,
    /// Remove an existing destination first.
    replace: bool,
}

/// A paste that is waiting for the user to decide what to do with each name clash.
struct PendingPaste {
    cut: bool,
    /// Items that need no decision (or were already decided).
    ready: Vec<PasteItem>,
    /// (source, destination) pairs whose destination is already taken; the front one is being asked about.
    todo: VecDeque<(PathBuf, PathBuf)>,
    /// "Do this for all remaining conflicts" was ticked.
    apply_all: bool,
}

fn occupied(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// "name (n).ext" with the lowest n that is free on disk and not claimed by this paste.
fn lowest_free_name(dest: &Path, taken: &HashSet<PathBuf>, is_dir: bool) -> PathBuf {
    let parent = dest.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = if is_dir {
        (name.clone(), String::new())
    } else {
        match name.rfind('.') {
            Some(i) if i > 0 => (name[..i].to_owned(), name[i..].to_owned()),
            _ => (name.clone(), String::new()),
        }
    };
    let mut n = 1u32;
    loop {
        let cand = parent.join(format!("{stem} ({n}){ext}"));
        if !occupied(&cand) && !taken.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

// ----------------------------------------------------------------------------------------------
// Tiling: up to four tabs side by side, built by dragging a tab onto the edge of a pane.

enum Tile {
    Leaf(u64),
    Split {
        /// true: a | b (left/right), false: a over b (top/bottom).
        side_by_side: bool,
        ratio: f32,
        a: Box<Tile>,
        b: Box<Tile>,
    },
}

impl Tile {
    fn leaf_ids(&self, out: &mut Vec<u64>) {
        match self {
            Tile::Leaf(id) => out.push(*id),
            Tile::Split { a, b, .. } => {
                a.leaf_ids(out);
                b.leaf_ids(out);
            }
        }
    }

    fn contains(&self, id: u64) -> bool {
        let mut v = Vec::new();
        self.leaf_ids(&mut v);
        v.contains(&id)
    }

    fn first_leaf(&self) -> u64 {
        match self {
            Tile::Leaf(id) => *id,
            Tile::Split { a, .. } => a.first_leaf(),
        }
    }

    fn map_leaves(&mut self, f: &mut dyn FnMut(u64) -> u64) {
        match self {
            Tile::Leaf(id) => *id = f(*id),
            Tile::Split { a, b, .. } => {
                a.map_leaves(f);
                b.map_leaves(f);
            }
        }
    }

    fn replace(&mut self, old: u64, new: u64) {
        self.map_leaves(&mut |id| if id == old { new } else { id });
    }

    fn swap(&mut self, x: u64, y: u64) {
        self.map_leaves(&mut |id| {
            if id == x {
                y
            } else if id == y {
                x
            } else {
                id
            }
        });
    }

    /// Turns the leaf `target` into a split holding `target` and `new`.
    fn split_leaf(&mut self, target: u64, new: u64, new_first: bool, side_by_side: bool) -> bool {
        match self {
            Tile::Leaf(x) => {
                if *x != target {
                    return false;
                }
                let (a, b) = if new_first {
                    (new, target)
                } else {
                    (target, new)
                };
                *self = Tile::Split {
                    side_by_side,
                    ratio: 0.5,
                    a: Box::new(Tile::Leaf(a)),
                    b: Box::new(Tile::Leaf(b)),
                };
                true
            }
            Tile::Split { a, b, .. } => {
                a.split_leaf(target, new, new_first, side_by_side)
                    || b.split_leaf(target, new, new_first, side_by_side)
            }
        }
    }
}

/// Removes a leaf; its sibling takes over the freed space.
fn remove_leaf(t: Tile, id: u64) -> Option<Tile> {
    match t {
        Tile::Leaf(x) => {
            if x == id {
                None
            } else {
                Some(Tile::Leaf(x))
            }
        }
        Tile::Split {
            side_by_side,
            ratio,
            a,
            b,
        } => match (remove_leaf(*a, id), remove_leaf(*b, id)) {
            (Some(a), Some(b)) => Some(Tile::Split {
                side_by_side,
                ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

fn drop_zone(r: Rect, p: egui::Pos2, can_split: bool) -> DropZone {
    if !can_split || r.width() < 1.0 || r.height() < 1.0 {
        return DropZone::Center;
    }
    let (u, v) = ((p.x - r.left()) / r.width(), (p.y - r.top()) / r.height());
    let (dl, dr, dt, db) = (u, 1.0 - u, v, 1.0 - v);
    let near = dl.min(dr).min(dt).min(db);
    if near > 0.28 {
        DropZone::Center
    } else if near == dl {
        DropZone::Left
    } else if near == dr {
        DropZone::Right
    } else if near == dt {
        DropZone::Top
    } else {
        DropZone::Bottom
    }
}

fn zone_rect(r: Rect, z: DropZone) -> Rect {
    match z {
        DropZone::Center => r,
        DropZone::Left => Rect::from_min_max(r.min, pos2(r.center().x, r.bottom())),
        DropZone::Right => Rect::from_min_max(pos2(r.center().x, r.top()), r.max),
        DropZone::Top => Rect::from_min_max(r.min, pos2(r.right(), r.center().y)),
        DropZone::Bottom => Rect::from_min_max(pos2(r.left(), r.center().y), r.max),
    }
}

struct Props {
    path: PathBuf,
    name: String,
    is_dir: bool,
    kind: String,
    size: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    readonly: bool,
    hidden: bool,
    stats: Option<FolderStats>,
    stats_gen: u64,
}

enum AppMsg {
    Listing {
        tab: u64,
        seq: u64,
        result: Result<Vec<Entry>, String>,
    },
    Drives(Vec<DriveInfo>),
    Job(Result<String, String>),
    Stats(u64, FolderStats),
    Trash {
        tab: u64,
        seq: u64,
        result: Result<Vec<trashbin::TrashRow>, String>,
    },
}

enum Action {
    Navigate(PathBuf),
    NewTabAt(PathBuf),
    NewTab,
    CloseTab(usize),
    ClosePane(u64),
    OpenTerminal(PathBuf),
    SwitchTab(usize),
    StepTab(i32),
    Back,
    Forward,
    Up,
    Refresh,
    GoToAddress(String),
    FocusAddress,
    FocusSearch,
    Open(PathBuf),
    Copy(Vec<PathBuf>),
    Cut(Vec<PathBuf>),
    CopyPath(Vec<PathBuf>),
    Paste,
    Rename(PathBuf),
    CommitRename(PathBuf, String),
    CancelRename,
    Delete(Vec<PathBuf>),
    NewFolder,
    Properties(PathBuf),
    OpenContaining(PathBuf),
    PeaOpen(PathBuf),
    PeaAdd(PathBuf),
    ToggleHidden,
    OpenSettings,
    OpenRecycleBin,
    MoveSelection(i32),
    SelectEdge(bool),
    TrashRestoreSelected,
    TrashPurgeSelected,
    TrashEmpty,
    TrashSelectAll,
    SelectAll,
}

#[derive(Default)]
struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    fn poke(&mut self) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = Some(now);
    }
    fn pending(&self) -> bool {
        self.first.is_some()
    }
    fn ready(&mut self, quiet: Duration, max: Duration) -> bool {
        if let (Some(f), Some(l)) = (self.first, self.last) {
            if l.elapsed() >= quiet || f.elapsed() >= max {
                self.first = None;
                self.last = None;
                return true;
            }
        }
        false
    }
}

// ==============================================================================================
// Listing

fn read_listing(dir: &Path, show_hidden: bool, fmt: DateFormat) -> Result<Vec<Entry>, String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("Could not read this folder: {e}"))?;
    let mut out = Vec::with_capacity(256);
    for item in rd.flatten() {
        // DirEntry::metadata() is free on Windows and carries the real attributes of junctions.
        let Ok(md) = item.metadata() else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && is_hidden_entry(&name, &md) {
            continue;
        }
        let path = item.path();
        let is_link = md.file_type().is_symlink();
        let target = if is_link {
            fs::metadata(&path).ok()
        } else {
            None
        };
        let eff = target.as_ref().unwrap_or(&md);
        let is_dir = eff.is_dir();
        let modified = eff.modified().ok().or_else(|| md.modified().ok());
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        let (type_text, icon) = if is_dir {
            ("File folder".to_owned(), "folder")
        } else {
            match &ext {
                Some(e) => (format!("{} file", e.to_uppercase()), icon_for_extension(e)),
                None => ("File".to_owned(), "file"),
            }
        };
        out.push(Entry {
            name_lc: name.to_lowercase(),
            name,
            path,
            is_dir,
            is_link,
            size: if is_dir { 0 } else { eff.len() },
            modified,
            date_text: format_date(modified, fmt, false),
            type_text,
            icon,
            dir_size: None,
        });
    }
    Ok(out)
}

fn rebuild_view(tab: &mut Tab) {
    let q = tab.search.trim().to_lowercase();
    let entries = &tab.entries;
    tab.view.clear();
    tab.view
        .extend((0..entries.len()).filter(|&i| q.is_empty() || entries[i].name_lc.contains(&q)));
    let (col, asc) = (tab.sort_col, tab.sort_asc);
    tab.view.sort_by(|&a, &b| {
        let (ea, eb) = (&entries[a], &entries[b]);
        let d = eb.is_dir.cmp(&ea.is_dir); // folders always first
        if d != Ordering::Equal {
            return d;
        }
        let o = match col {
            SortColumn::Name => natural_cmp(&ea.name_lc, &eb.name_lc),
            SortColumn::Modified => ea.modified.cmp(&eb.modified),
            SortColumn::Type => ea
                .type_text
                .cmp(&eb.type_text)
                .then_with(|| natural_cmp(&ea.name_lc, &eb.name_lc)),
            SortColumn::Size => ea.sort_size().cmp(&eb.sort_size()),
        };
        if asc {
            o
        } else {
            o.reverse()
        }
    });
    tab.view_dirty = false;
}

fn load_drives() -> Vec<DriveInfo> {
    let mut drives: Vec<DriveInfo> = Disks::new_with_refreshed_list()
        .list()
        .iter()
        .map(|d| DriveInfo {
            mount_point: d.mount_point().to_path_buf(),
            name: d.name().to_string_lossy().into_owned(),
            total: d.total_space(),
            available: d.available_space(),
        })
        .collect();
        //sort drives by drive letter
        drives.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
        drives
}

fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => match std::env::var(&after[..j]) {
                Ok(v) => {
                    out.push_str(&v);
                    rest = &after[j + 1..];
                }
                Err(_) => {
                    out.push('%');
                    rest = after;
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn build_props(path: &Path) -> Props {
    let md = fs::metadata(path).ok();
    let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_uppercase());
    Props {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        is_dir,
        kind: if is_dir {
            "File folder".into()
        } else {
            ext.map(|e| format!("{e} file"))
                .unwrap_or_else(|| "File".into())
        },
        size: md.as_ref().map(|m| m.len()).unwrap_or(0),
        created: md.as_ref().and_then(|m| m.created().ok()),
        modified: md.as_ref().and_then(|m| m.modified().ok()),
        accessed: md.as_ref().and_then(|m| m.accessed().ok()),
        readonly: md.as_ref().is_some_and(|m| m.permissions().readonly()),
        hidden: md.as_ref().is_some_and(|m| attributes(m) & 0x2 != 0),
        stats: None,
        stats_gen: 0,
    }
}

// ==============================================================================================
// App

struct Explorer {
    ctx: egui::Context,
    settings: Settings,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,

    drives: Vec<DriveInfo>,
    last_drives: Instant,
    drives_inflight: bool,
    quick: Vec<(&'static str, &'static str, PathBuf)>,
    clipboard: Option<(Vec<PathBuf>, bool)>,
    peazip: Option<PathBuf>,

    msg_tx: Sender<AppMsg>,
    msg_rx: Receiver<AppMsg>,
    idx_rx: Receiver<IndexMsg>,
    indexer: Indexer,
    watcher: Option<DirWatcher>,
    list_debounce: Debounce,
    size_debounce: Debounce,
    index_dir: PathBuf,

    jobs_running: usize,
    job_label: String,
    conflict: Option<PendingPaste>,
    props: Option<Props>,
    props_gen: Arc<AtomicU64>,
    settings_open: bool,
    settings_dirty: bool,

    sys_accent: Option<Accent>,
    text_scale: f32,
    last_poll: Instant,
    applied_style: Option<(f32, Accent)>,
    backdrop_theme: Option<egui::Theme>,
    mica: bool,
    m: Metrics,
    pal: Palette,
    last_title: String,
    last_save: Instant,
    first_anim: f32,
    font_edit: Option<f32>,
    confirm: Option<ConfirmPurge>,
    sidebar_rect: Option<Rect>,

    /// None: the normal single-tab view. Some: up to four tabs tiled in the file area.
    tiles: Option<Tile>,
    /// Tab (by id) currently being dragged out of the tab strip.
    tab_drag: Option<u64>,
    /// What the folder-size indexer was last started for; avoids restarting an identical scan.
    index_sig: Option<(u64, usize)>,
    reindex_force: bool,

    /// Other SlopExplore windows (tab hand-over).
    peers: Peers,
    own_info: Option<WinInfo>,
    info_stamp: Instant,
    /// Windows transparency effects available (off in Battery Saver / when disabled in Settings).
    transparency: bool,
    backdrop_transp: Option<bool>,
    /// (when checked, whether another SlopExplore window is under the pointer) while dragging a tab.
    peer_under: (Instant, bool),
}

impl Explorer {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        egui_extras::install_image_loaders(&ctx);
        install_fonts(&ctx);

        let settings = load_settings();
        ctx.set_theme(match settings.theme {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        });

        let (msg_tx, msg_rx) = mpsc::channel();
        let (idx_tx, idx_rx) = mpsc::channel();
        let index_dir = settings
            .index_dir
            .clone()
            .filter(|p| p.is_dir())
            .unwrap_or_else(app_dir);
        let indexer = Indexer::new(ctx.clone(), idx_tx, &index_dir);
        let watcher = DirWatcher::new(ctx.clone());

        let start = START_DIR
            .get()
            .cloned()
            .filter(|p| p.is_dir())
            .or_else(|| dirs::home_dir().filter(|p| p.is_dir()))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let mut quick = Vec::new();
        for (icon, label, path) in [
            ("home", "Home", dirs::home_dir()),
            ("desktop", "Desktop", dirs::desktop_dir()),
            ("downloads", "Downloads", dirs::download_dir()),
            ("documents", "Documents", dirs::document_dir()),
            ("pictures", "Pictures", dirs::picture_dir()),
            ("music", "Music", dirs::audio_dir()),
            ("videos", "Videos", dirs::video_dir()),
        ] {
            if let Some(p) = path.filter(|p| p.is_dir()) {
                quick.push((icon, label, p));
            }
        }

        let mut app = Self {
            ctx: ctx.clone(),
            settings,
            tabs: vec![Tab::new(1, start)],
            active: 0,
            next_tab_id: 2,
            drives: Vec::new(),
            last_drives: Instant::now() - Duration::from_secs(60),
            drives_inflight: false,
            quick,
            clipboard: None,
            peazip: find_peazip(),
            msg_tx,
            msg_rx,
            idx_rx,
            indexer,
            watcher,
            list_debounce: Debounce::default(),
            size_debounce: Debounce::default(),
            index_dir,
            jobs_running: 0,
            job_label: String::new(),
            conflict: None,
            props: None,
            props_gen: Arc::new(AtomicU64::new(0)),
            settings_open: false,
            settings_dirty: false,
            sys_accent: system_accent(),
            text_scale: system_text_scale(),
            last_poll: Instant::now(),
            applied_style: None,
            backdrop_theme: None,
            mica: false,
            m: Metrics::new(14.0, 1.0),
            pal: Palette::new(egui::Theme::Dark, Accent::fallback(), false),
            last_title: String::new(),
            last_save: Instant::now(),
            first_anim: 1.0,
            font_edit: None,
            confirm: None,
            sidebar_rect: None,
            tiles: None,
            tab_drag: None,
            index_sig: None,
            reindex_force: false,
            peers: Peers::new(ctx.clone(), &app_dir()),
            own_info: None,
            info_stamp: Instant::now(),
            transparency: system_transparency(),
            backdrop_transp: None,
            peer_under: (Instant::now(), false),
        };
        app.load_tab(0);
        app.on_active_changed();
        app
    }

    // ------------------------------------------------------------------ navigation

    fn load_tab(&mut self, ti: usize) {
        let (show_hidden, fmt) = (self.settings.show_hidden, self.settings.date_format);
        let tab = &mut self.tabs[ti];
        tab.load_gen += 1;
        tab.loading = true;
        tab.error = None;
        let (id, seq, dir) = (tab.id, tab.load_gen, tab.dir.clone());
        let (tx, ctx) = (self.msg_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = read_listing(&dir, show_hidden, fmt);
            let _ = tx.send(AppMsg::Listing {
                tab: id,
                seq,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn open_trash(&mut self, ti: usize) {
        let tab = &mut self.tabs[ti];
        if tab.trash.is_none() {
            let cur = tab.dir.clone();
            tab.back.push(cur);
            tab.fwd.clear();
            tab.trash = Some(TrashState::new());
            tab.search.clear();
            tab.clear_selection();
            tab.rename = None;
            tab.editing_addr = false;
            tab.view_dirty = true;
        }
        self.load_trash(ti);
    }

    fn load_trash(&mut self, ti: usize) {
        let fmt = self.settings.date_format;
        let tab = &mut self.tabs[ti];
        let Some(tr) = tab.trash.as_mut() else { return };
        tr.seq += 1;
        tr.loading = true;
        let (id, seq) = (tab.id, tr.seq);
        let (tx, ctx) = (self.msg_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = trashbin::load(fmt);
            let _ = tx.send(AppMsg::Trash {
                tab: id,
                seq,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn trash_items(&self, ti: usize, all: bool) -> Vec<trashbin::Item> {
        self.tabs[ti]
            .trash
            .as_ref()
            .map(|tr| {
                if all {
                    tr.rows.iter().map(|r| r.item.clone()).collect()
                } else {
                    tr.selected
                        .iter()
                        .filter_map(|&i| tr.rows.get(i))
                        .map(|r| r.item.clone())
                        .collect()
                }
            })
            .unwrap_or_default()
    }

    fn reload_all(&mut self) {
        for i in 0..self.tabs.len() {
            self.load_tab(i);
        }
    }

    fn navigate(&mut self, ti: usize, path: PathBuf) {
        let path = match dunce::canonicalize(&path) {
            Ok(p) if p.is_dir() => p,
            Ok(_) => {
                self.tabs[ti].error = Some("That path is not a folder.".to_owned());
                return;
            }
            Err(e) => {
                self.tabs[ti].error = Some(format!("Could not open folder: {e}"));
                return;
            }
        };
        let tab = &mut self.tabs[ti];
        let in_trash = tab.trash.is_some();
        if path == tab.dir {
            if in_trash {
                // The Recycle Bin is a view *over* `tab.dir`, so asking for that same folder
                // (e.g. Home right after opening the bin from Home) must leave the bin instead of
                // being ignored. open_trash() already pushed `dir` onto the back stack; undo that.
                if tab.back.last() == Some(&path) {
                    tab.back.pop();
                }
                tab.fwd.clear();
                self.set_dir(ti, path);
            } else {
                tab.address = path.display().to_string();
            }
            return;
        }
        if !in_trash {
            // (in the bin, open_trash() has already recorded the folder we came from)
            let prev = tab.dir.clone();
            tab.back.push(prev);
        }
        tab.fwd.clear();
        self.set_dir(ti, path);
    }

    fn set_dir(&mut self, ti: usize, path: PathBuf) {
        let tab = &mut self.tabs[ti];
        tab.address = path.display().to_string();
        tab.dir = path;
        tab.search.clear();
        tab.clear_selection();
        tab.rename = None;
        tab.editing_addr = false;
        tab.trash = None;
        tab.entries.clear();
        tab.lookup.clear();
        tab.view.clear();
        tab.view_dirty = true;
        self.load_tab(ti);
        if ti == self.active {
            self.on_active_changed();
        }
    }

    fn on_active_changed(&mut self) {
        let dir = self.tabs[self.active].dir.clone();
        if let Some(w) = &mut self.watcher {
            w.watch(&dir);
        }
        self.start_indexing(false);
    }

    /// Indices of the tabs currently on screen (all tiled tabs, or just the active one).
    fn visible_tabs(&self) -> Vec<usize> {
        if let Some(t) = &self.tiles {
            let mut ids = Vec::new();
            t.leaf_ids(&mut ids);
            let v: Vec<usize> = ids
                .iter()
                .filter_map(|id| self.tabs.iter().position(|x| x.id == *id))
                .collect();
            if !v.is_empty() {
                return v;
            }
        }
        vec![self.active]
    }

    /// Starts the folder-size scan for every visible tab. A scan for exactly the same set of
    /// folders is not restarted unless `force` is set (restarting throws away the work already done).
    fn start_indexing(&mut self, force: bool) {
        let mut paths = Vec::new();
        let mut acc = 0u64;
        for ti in self.visible_tabs() {
            let tab = &mut self.tabs[ti];
            let mut h = DefaultHasher::new();
            tab.id.hash(&mut h);
            tab.dir.hash(&mut h);
            acc ^= h.finish();
            for e in tab.entries.iter_mut().filter(|e| e.is_dir && !e.is_link) {
                if e.dir_size.is_none() {
                    e.dir_size = self.indexer.lookup(&e.path);
                }
                let mut h = DefaultHasher::new();
                e.path.hash(&mut h);
                acc ^= h.finish().rotate_left(13);
                paths.push(e.path.clone());
            }
        }
        let sig = (acc, paths.len());
        if !force && self.index_sig == Some(sig) {
            return;
        }
        self.index_sig = Some(sig);
        self.indexer.start(paths); // also cancels a scan that belonged to the previous folder
    }

    fn new_tab(&mut self, dir: PathBuf) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        if let Some(tree) = self.tiles.as_mut() {
            // In tiled mode the new tab takes over the focused pane.
            let cur = self.tabs[self.active].id;
            tree.replace(cur, id);
        }
        self.tabs.push(Tab::new(id, dir));
        self.active = self.tabs.len() - 1;
        self.load_tab(self.active);
        self.on_active_changed();
    }

    /// Makes tab `i` the focused one. In tiled mode a tab that is not on screen replaces the focused pane.
    fn activate_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let new_id = self.tabs[i].id;
        let mut shown = false;
        if let Some(tree) = self.tiles.as_mut() {
            if !tree.contains(new_id) {
                let cur = self.tabs[self.active].id;
                tree.replace(cur, new_id);
                shown = true;
            }
        }
        if i != self.active || shown {
            self.active = i;
            self.on_active_changed();
            if shown {
                self.load_tab(i);
            }
        }
    }

    /// Keeps the tile tree consistent: drops panes whose tab is gone, turns a lone pane back into the
    /// normal view, and makes sure the focused tab is one of the visible ones.
    fn fix_tiles(&mut self) {
        let Some(tree) = self.tiles.take() else {
            return;
        };
        let mut ids = Vec::new();
        tree.leaf_ids(&mut ids);
        let mut tree = Some(tree);
        for id in ids {
            if !self.tabs.iter().any(|t| t.id == id) {
                tree = tree.and_then(|t| remove_leaf(t, id));
            }
        }
        let Some(tree) = tree else {
            return;
        };
        if let Tile::Leaf(id) = &tree {
            if let Some(i) = self.tabs.iter().position(|t| t.id == *id) {
                if i != self.active {
                    self.active = i;
                    self.on_active_changed();
                }
            }
            return; // a single pane is just the normal view
        }
        let active_id = self.tabs[self.active].id;
        let first = tree.first_leaf();
        let has_active = tree.contains(active_id);
        self.tiles = Some(tree);
        if !has_active {
            if let Some(i) = self.tabs.iter().position(|t| t.id == first) {
                self.active = i;
                self.on_active_changed();
            }
        }
    }

    /// A tab was dropped on a pane: on its edge it splits the pane, in the middle it takes the pane's place.
    fn drop_tab(&mut self, dragged: u64, mut target: u64, zone: DropZone) {
        let Some(di) = self.tabs.iter().position(|t| t.id == dragged) else {
            return;
        };
        if dragged == target && zone != DropZone::Center && self.tiles.is_none() {
            // Dragging the tab you are looking at onto the edge of the view: it becomes the new
            // pane, and the pane it leaves behind shows another tab (a fresh one if there is none).
            let other = self
                .tabs
                .iter()
                .skip(di + 1)
                .chain(self.tabs.iter().take(di))
                .next()
                .map(|t| t.id);
            target = match other {
                Some(id) => id,
                None => {
                    let id = self.next_tab_id;
                    self.next_tab_id += 1;
                    let dir = self.tabs[di].dir.clone();
                    self.tabs.push(Tab::new(id, dir));
                    let last = self.tabs.len() - 1;
                    self.load_tab(last);
                    id
                }
            };
        }
        let mut tree = self.tiles.take().unwrap_or(Tile::Leaf(target));
        if dragged != target {
            if zone == DropZone::Center {
                if tree.contains(dragged) {
                    tree.swap(dragged, target);
                } else {
                    tree.replace(target, dragged);
                }
            } else {
                let mut ids = Vec::new();
                tree.leaf_ids(&mut ids);
                let already = ids.contains(&dragged);
                if already || ids.len() < 4 {
                    let base = if already {
                        remove_leaf(tree, dragged)
                    } else {
                        Some(tree)
                    };
                    let Some(mut base) = base else {
                        return;
                    };
                    let (new_first, side_by_side) = match zone {
                        DropZone::Left => (true, true),
                        DropZone::Right => (false, true),
                        DropZone::Top => (true, false),
                        _ => (false, false),
                    };
                    base.split_leaf(target, dragged, new_first, side_by_side);
                    tree = base;
                }
            }
        }
        self.tiles = Some(tree);
        if di != self.active {
            self.active = di;
            self.on_active_changed();
        }
        self.load_tab(di);
        self.fix_tiles();
    }

    fn open_path(&mut self, path: &Path) {
        match fs::metadata(path) {
            Ok(m) if m.is_dir() => self.navigate(self.active, path.to_path_buf()),
            Ok(_) => {
                if let Err(e) = open::that(path) {
                    self.tabs[self.active].error = Some(format!("Could not open file: {e}"));
                }
            }
            Err(e) => self.tabs[self.active].error = Some(format!("Could not access item: {e}")),
        }
    }

    // ------------------------------------------------------------------ background work

    fn run_job(
        &mut self,
        label: &str,
        f: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        self.jobs_running += 1;
        self.job_label = label.to_owned();
        let (tx, ctx) = (self.msg_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            // A panic in the job must still report back, otherwise the status bar spins forever.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
                Err("The operation failed unexpectedly (details in crash.log).".to_owned())
            });
            let _ = tx.send(AppMsg::Job(r));
            ctx.request_repaint();
        });
    }

    /// Runs the decided paste items as one background job.
    fn run_paste(&mut self, items: Vec<PasteItem>, cut: bool) {
        if items.is_empty() {
            return;
        }
        if cut {
            self.clipboard = None;
        }
        self.run_job(if cut { "Moving…" } else { "Copying…" }, move || {
            let mut errors: Vec<String> = Vec::new();
            for it in items {
                let name = it
                    .dst
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if it.replace && occupied(&it.dst) {
                    if let Err(e) = remove_existing(&it.dst) {
                        errors.push(format!("Could not replace “{name}”: {e}"));
                        continue;
                    }
                }
                let r = if cut {
                    move_item(&it.src, &it.dst)
                } else {
                    copy_item(&it.src, &it.dst)
                };
                if let Err(e) = r {
                    errors.push(format!("Could not paste “{name}”: {e}"));
                }
            }
            if errors.is_empty() {
                Ok(String::new())
            } else {
                let more = errors.len().saturating_sub(2);
                let mut text = errors.into_iter().take(2).collect::<Vec<_>>().join("  |  ");
                if more > 0 {
                    text.push_str(&format!("  (+{more} more)"));
                }
                Err(text)
            }
        });
    }

    fn paste(&mut self) {
        if self.conflict.is_some() {
            return; // a question is already on screen
        }
        let Some((sources, cut)) = self.clipboard.clone() else {
            return;
        };
        let dir = self.tabs[self.active].dir.clone();
        let mut ready: Vec<PasteItem> = Vec::new();
        let mut todo: VecDeque<(PathBuf, PathBuf)> = VecDeque::new();
        let mut claimed: HashSet<PathBuf> = HashSet::new();
        for source in sources {
            let Some(name) = source.file_name() else {
                continue;
            };
            let dest = dir.join(name);
            if cut && source == dest {
                continue; // moving an item onto itself is a no-op
            }
            if source.is_dir() && dir.starts_with(&source) {
                self.tabs[self.active].error = Some(format!(
                    "Can't {} “{}” into itself or one of its subfolders.",
                    if cut { "move" } else { "copy" },
                    name.to_string_lossy()
                ));
                continue;
            }
            // Same-folder copies land here too (source == dest): they are asked about like any other clash.
            if occupied(&dest) || claimed.contains(&dest) {
                todo.push_back((source, dest));
            } else {
                claimed.insert(dest.clone());
                ready.push(PasteItem {
                    src: source,
                    dst: dest,
                    replace: false,
                });
            }
        }
        if todo.is_empty() {
            self.run_paste(ready, cut);
        } else {
            self.conflict = Some(PendingPaste {
                cut,
                ready,
                todo,
                apply_all: false,
            });
        }
    }

    fn refresh_drives(&mut self) {
        self.drives_inflight = true;
        self.last_drives = Instant::now();
        let (tx, ctx) = (self.msg_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(AppMsg::Drives(load_drives()));
            ctx.request_repaint();
        });
    }

    fn pump_messages(&mut self) {
        let active_id = self.tabs[self.active].id;
        let mut reindex = false;
        let mut reindex_force = false;

        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                AppMsg::Listing { tab, seq, result } => {
                    if let Some(t) = self
                        .tabs
                        .iter_mut()
                        .find(|t| t.id == tab && t.load_gen == seq)
                    {
                        t.loading = false;
                        match result {
                            Ok(mut entries) => {
                                for e in entries.iter_mut().filter(|e| e.is_dir && !e.is_link) {
                                    e.dir_size = self.indexer.lookup(&e.path);
                                }
                                t.lookup = entries
                                    .iter()
                                    .enumerate()
                                    .map(|(i, e)| (e.path.clone(), i))
                                    .collect();
                                t.entries = entries;
                                t.view_dirty = true;
                                if let Some(p) = t.pending_rename.take() {
                                    if let Some(&i) = t.lookup.get(&p) {
                                        t.rename = Some(RenameState {
                                            text: t.entries[i].name.clone(),
                                            is_dir: t.entries[i].is_dir,
                                            path: p.clone(),
                                            focus_pending: true,
                                        });
                                        t.select_only(p);
                                    }
                                }
                                let lookup = &t.lookup;
                                t.selected.retain(|p| lookup.contains_key(p));
                                if t.anchor.as_ref().is_some_and(|a| !lookup.contains_key(a)) {
                                    t.anchor = None;
                                }
                                // `view` holds indices into `entries`; they are stale as soon as the
                                // list changes. Rebuild now, before anything else touches them
                                // (deleting a file used to leave an out-of-range index behind).
                                rebuild_view(t);
                            }
                            Err(e) => {
                                t.entries.clear();
                                t.lookup.clear();
                                t.view.clear();
                                t.error = Some(e);
                            }
                        }
                        if t.id == active_id {
                            reindex = true;
                        }
                    }
                }
                AppMsg::Trash { tab, seq, result } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        if let Some(tr) = t.trash.as_mut() {
                            if tr.seq == seq {
                                tr.loading = false;
                                tr.selected.clear();
                                tr.anchor = None;
                                match result {
                                    Ok(rows) => {
                                        tr.rows = rows;
                                        rebuild_trash_view(tr, &t.search);
                                        t.view_dirty = false;
                                    }
                                    Err(e) => {
                                        tr.rows.clear();
                                        tr.view.clear();
                                        tr.error = Some(e);
                                    }
                                }
                            }
                        }
                    }
                }
                AppMsg::Drives(d) => {
                    self.drives = d;
                    self.drives_inflight = false;
                }
                AppMsg::Job(result) => {
                    self.jobs_running = self.jobs_running.saturating_sub(1);
                    if self.jobs_running == 0 {
                        self.job_label.clear();
                    }
                    let active = self.active;
                    if let Err(e) = result {
                        match self.tabs[active].trash.as_mut() {
                            Some(tr) => tr.error = Some(e),
                            None => self.tabs[active].error = Some(e),
                        }
                    }
                    // Refresh every pane on screen: a paste may have landed in a neighbouring pane.
                    for ti in self.visible_tabs() {
                        if self.tabs[ti].trash.is_some() {
                            self.load_trash(ti);
                        } else {
                            self.load_tab(ti);
                        }
                    }
                }
                AppMsg::Stats(seq, stats) => {
                    if let Some(p) = &mut self.props {
                        if p.stats_gen == seq {
                            p.stats = Some(stats);
                        }
                    }
                }
            }
        }

        while let Ok(IndexMsg::Size(path, size)) = self.idx_rx.try_recv() {
            let Some(parent) = path.parent() else {
                continue;
            };
            for t in &mut self.tabs {
                if t.dir == parent {
                    if let Some(&i) = t.lookup.get(&path) {
                        t.entries[i].dir_size = Some(size);
                        if t.sort_col == SortColumn::Size {
                            t.view_dirty = true;
                        }
                    }
                }
            }
        }

        // Live updates from the file watcher.
        let changed = self.watcher.as_ref().map(|w| w.take()).unwrap_or_default();
        if !changed.is_empty() {
            let dir = self.tabs[self.active].dir.clone();
            for p in changed {
                if p.as_os_str().is_empty() {
                    // Too many changes to list: refresh everything.
                    self.list_debounce.poke();
                    self.size_debounce.poke();
                    continue;
                }
                if p.starts_with(&self.index_dir) {
                    continue; // our own cache file
                }
                self.indexer.invalidate(&p);
                if p.parent() == Some(dir.as_path()) || p == dir {
                    self.list_debounce.poke();
                } else if p.starts_with(&dir) {
                    self.size_debounce.poke();
                }
            }
        }

        // Folders other windows dragged over to us arrive as new tabs.
        for dir in self.peers.take_inbox() {
            if dir.is_dir() {
                self.new_tab(dir);
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        if self
            .list_debounce
            .ready(Duration::from_millis(250), Duration::from_millis(1500))
        {
            self.load_tab(self.active);
        }
        // Long quiet period for sizes: a download writing to a folder must not trigger a rescan every few ms.
        if self
            .size_debounce
            .ready(Duration::from_millis(1500), Duration::from_secs(10))
        {
            reindex = true;
            reindex_force = true; // something below the folder changed: sizes really are stale
        }
        if self.list_debounce.pending() || self.size_debounce.pending() {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
        if reindex {
            let force = reindex_force || std::mem::take(&mut self.reindex_force);
            self.start_indexing(force);
        }

        // Probing every volume can block for seconds on a disconnected network drive or an empty
        // card reader, so do it rarely and only while the window is in use.
        if self.last_drives.elapsed() > Duration::from_secs(30)
            && !self.drives_inflight
            && self.ctx.input(|i| i.focused)
        {
            self.refresh_drives();
        }
        if self.indexer.is_dirty() {
            if self.last_save.elapsed() > Duration::from_secs(15) {
                self.indexer.save_if_dirty(&self.index_dir, false);
                self.last_save = Instant::now();
            } else {
                self.ctx.request_repaint_after(Duration::from_secs(16));
            }
        }
    }

    // ------------------------------------------------------------------ system integration

    fn sync_system(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.last_poll.elapsed() > Duration::from_secs(2) && ctx.input(|i| i.focused) {
            self.last_poll = Instant::now();
            self.text_scale = system_text_scale();
            self.sys_accent = system_accent();
            self.transparency = system_transparency();
        }
        let accent = if self.settings.use_system_accent {
            self.sys_accent.unwrap_or_else(Accent::fallback)
        } else {
            let c = self.settings.custom_accent;
            Accent::from_base(Color32::from_rgb(c[0], c[1], c[2]))
        };
        let font = self.settings.font_size
            * if self.settings.follow_text_scale {
                self.text_scale
            } else {
                1.0
            };
        self.m = Metrics::new(font, ctx.pixels_per_point());
        if self.applied_style != Some((font, accent)) {
            apply_style(ctx, &self.m, accent);
            self.applied_style = Some((font, accent));
        }
        let theme = ctx.theme();
        if self.backdrop_theme != Some(theme) || self.backdrop_transp != Some(self.transparency) {
            // Without transparency effects (Battery Saver etc.) Windows draws no Mica: paint opaque instead.
            self.mica = self.transparency && apply_backdrop(frame, theme == egui::Theme::Dark);
            style_frameless(frame);
            self.backdrop_theme = Some(theme);
            self.backdrop_transp = Some(self.transparency);
        }
        self.pal = Palette::new(theme, accent, self.mica);
    }

    // ------------------------------------------------------------------ shortcuts

    fn collect_shortcuts(&self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        use egui::{Key, Modifiers as M};
        let typing = ctx.wants_keyboard_input();
        let modal = self.settings_open
            || self.props.is_some()
            || self.conflict.is_some()
            || self.confirm.is_some();
        let tab = &self.tabs[self.active];
        let in_trash = tab.trash.is_some();
        let sel: Vec<PathBuf> = if in_trash {
            Vec::new()
        } else {
            tab.selected_paths()
        };
        let active = self.active;

        ctx.input_mut(|i| {
            if i.consume_key(M::CTRL | M::SHIFT, Key::Tab) {
                actions.push(Action::StepTab(-1));
            } else if i.consume_key(M::CTRL, Key::Tab) {
                actions.push(Action::StepTab(1));
            }
            if i.consume_key(M::CTRL, Key::T) {
                actions.push(Action::NewTab);
            }
            if i.consume_key(M::CTRL, Key::W) {
                actions.push(Action::CloseTab(active));
            }
            if i.consume_key(M::CTRL, Key::L) || i.consume_key(M::ALT, Key::D) {
                actions.push(Action::FocusAddress);
            }
            if i.consume_key(M::CTRL, Key::F) || i.consume_key(M::CTRL, Key::E) {
                actions.push(Action::FocusSearch);
            }
            if i.consume_key(M::NONE, Key::F5) {
                actions.push(Action::Refresh);
            }
            if i.consume_key(M::ALT, Key::ArrowLeft) {
                actions.push(Action::Back);
            }
            if i.consume_key(M::ALT, Key::ArrowRight) {
                actions.push(Action::Forward);
            }
            if i.consume_key(M::ALT, Key::ArrowUp) {
                actions.push(Action::Up);
            }
            // Mouse "back"/"forward" buttons: no need to travel to the toolbar.
            if i.pointer.button_pressed(egui::PointerButton::Extra1) {
                actions.push(Action::Back);
            }
            if i.pointer.button_pressed(egui::PointerButton::Extra2) {
                actions.push(Action::Forward);
            }
            if i.consume_key(M::CTRL | M::SHIFT, Key::N) {
                actions.push(Action::NewFolder);
            }

            if typing || modal {
                return;
            }
            if in_trash {
                if i.consume_key(M::NONE, Key::Delete) {
                    actions.push(Action::TrashPurgeSelected);
                }
                if i.consume_key(M::CTRL, Key::A) {
                    actions.push(Action::TrashSelectAll);
                }
            }
            if !in_trash && i.consume_key(M::CTRL, Key::A) {
                actions.push(Action::SelectAll);
            }
            let ev_copy = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            let ev_cut = i.events.iter().any(|e| matches!(e, egui::Event::Cut));
            let ev_paste = i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)));
            if !sel.is_empty() {
                if ev_copy || (i.modifiers.command && i.key_pressed(Key::C)) {
                    actions.push(Action::Copy(sel.clone()));
                }
                if ev_cut || (i.modifiers.command && i.key_pressed(Key::X)) {
                    actions.push(Action::Cut(sel.clone()));
                }
                if i.consume_key(M::NONE, Key::Enter) {
                    if sel.len() == 1 {
                        actions.push(Action::Open(sel[0].clone()));
                    } else {
                        // With several items selected, Enter opens the files (not the folders).
                        for p in sel.iter().filter(|p| !p.is_dir()).take(20) {
                            actions.push(Action::Open(p.clone()));
                        }
                    }
                }
                if sel.len() == 1 && i.consume_key(M::NONE, Key::F2) {
                    actions.push(Action::Rename(sel[0].clone()));
                }
                if i.consume_key(M::NONE, Key::Delete) {
                    actions.push(Action::Delete(sel.clone()));
                }
                if i.consume_key(M::ALT, Key::Enter) {
                    actions.push(Action::Properties(sel[0].clone()));
                }
            }
            if ev_paste || (i.modifiers.command && i.key_pressed(Key::V)) {
                actions.push(Action::Paste);
            }
            if i.consume_key(M::NONE, Key::Backspace) {
                actions.push(Action::Back);
            }
            if i.consume_key(M::NONE, Key::ArrowDown) {
                actions.push(Action::MoveSelection(1));
            }
            if i.consume_key(M::NONE, Key::ArrowUp) {
                actions.push(Action::MoveSelection(-1));
            }
            if i.consume_key(M::NONE, Key::PageDown) {
                actions.push(Action::MoveSelection(10));
            }
            if i.consume_key(M::NONE, Key::PageUp) {
                actions.push(Action::MoveSelection(-10));
            }
            if i.consume_key(M::NONE, Key::Home) {
                actions.push(Action::SelectEdge(false));
            }
            if i.consume_key(M::NONE, Key::End) {
                actions.push(Action::SelectEdge(true));
            }
        });
    }

    // ------------------------------------------------------------------ actions

    fn apply(&mut self, action: Action) {
        let ti = self.active;
        if self.tabs[ti].trash.is_some()
            && matches!(
                action,
                Action::NewFolder | Action::Paste | Action::Rename(_) | Action::CommitRename(..)
            )
        {
            return;
        }
        match action {
            Action::Navigate(p) => self.navigate(ti, p),
            Action::NewTabAt(p) => self.new_tab(p),
            Action::NewTab => {
                let home = dirs::home_dir().unwrap_or_else(|| self.tabs[ti].dir.clone());
                self.new_tab(home);
            }
            Action::CloseTab(i) => {
                if self.tabs.len() <= 1 {
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else if i < self.tabs.len() {
                    let closed_id = self.tabs[i].id;
                    self.tabs.remove(i);
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len() - 1;
                    } else if i < self.active {
                        self.active -= 1;
                    }
                    if let Some(t) = self.tiles.take() {
                        self.tiles = remove_leaf(t, closed_id);
                    }
                    self.on_active_changed();
                    self.fix_tiles();
                }
            }
            Action::ClosePane(id) => {
                if let Some(t) = self.tiles.take() {
                    self.tiles = remove_leaf(t, id);
                }
                self.fix_tiles();
            }
            Action::OpenTerminal(dir) => {
                if let Err(e) = open_terminal(&dir) {
                    self.tabs[ti].error = Some(format!("Could not open a terminal: {e}"));
                }
            }
            Action::SwitchTab(i) => self.activate_tab(i),
            Action::StepTab(d) => {
                let n = self.tabs.len() as i32;
                let next = (self.active as i32 + d).rem_euclid(n) as usize;
                self.activate_tab(next);
            }
            Action::Back => {
                let tab = &mut self.tabs[ti];
                if let Some(p) = tab.back.pop() {
                    let cur = tab.dir.clone();
                    tab.fwd.push(cur);
                    self.set_dir(ti, p);
                }
            }
            Action::Forward => {
                let tab = &mut self.tabs[ti];
                if let Some(p) = tab.fwd.pop() {
                    let cur = tab.dir.clone();
                    tab.back.push(cur);
                    self.set_dir(ti, p);
                }
            }
            Action::Up => {
                if let Some(parent) = self.tabs[ti].dir.parent().map(Path::to_path_buf) {
                    self.navigate(ti, parent);
                }
            }
            Action::Refresh => {
                if self.tabs[ti].trash.is_some() {
                    self.load_trash(ti);
                } else {
                    self.reindex_force = true; // F5 also recalculates folder sizes
                    self.load_tab(ti);
                }
            }
            Action::GoToAddress(s) => {
                let typed = expand_env(s.trim().trim_matches('"'));
                if !typed.is_empty() {
                    self.navigate(ti, PathBuf::from(typed));
                }
            }
            Action::FocusAddress => {
                let tab = &mut self.tabs[ti];
                tab.address = tab.dir.display().to_string();
                tab.editing_addr = true;
                tab.addr_focus = true;
            }
            Action::FocusSearch => self
                .ctx
                .memory_mut(|m| m.request_focus(Id::new("search_box"))),
            Action::Open(p) => self.open_path(&p),
            Action::Copy(p) => {
                if !p.is_empty() {
                    self.clipboard = Some((p, false));
                }
            }
            Action::Cut(p) => {
                if !p.is_empty() {
                    self.clipboard = Some((p, true));
                }
            }
            Action::CopyPath(p) => {
                let text = p
                    .iter()
                    .map(|x| x.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.ctx.copy_text(text);
            }
            Action::Paste => self.paste(),
            Action::Rename(p) => {
                let tab = &mut self.tabs[ti];
                if let Some(&i) = tab.lookup.get(&p) {
                    tab.rename = Some(RenameState {
                        text: tab.entries[i].name.clone(),
                        is_dir: tab.entries[i].is_dir,
                        path: p.clone(),
                        focus_pending: true,
                    });
                    tab.select_only(p);
                    tab.scroll_to = tab.view.iter().position(|&v| v == i);
                }
            }
            Action::CancelRename => self.tabs[ti].rename = None,
            Action::CommitRename(p, new_name) => {
                self.tabs[ti].rename = None;
                let new_name = new_name.trim().to_owned();
                let old_name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if new_name.is_empty() || new_name == old_name {
                    return;
                }
                if new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
                    self.tabs[ti].error = Some(
                        "A name can't contain any of these characters: \\ / : * ? \" < > |".into(),
                    );
                    return;
                }
                let Some(parent) = p.parent() else { return };
                let dest = parent.join(&new_name);
                match fs::rename(&p, &dest) {
                    Ok(()) => {
                        self.indexer.invalidate(&p);
                        self.tabs[ti].select_only(dest);
                        self.load_tab(ti);
                    }
                    Err(e) => self.tabs[ti].error = Some(format!("Could not rename item: {e}")),
                }
            }
            Action::Delete(p) => {
                if p.is_empty() {
                    return;
                }
                self.run_job("Moving to Recycle Bin…", move || {
                    trash::delete_all(&p)
                        .map(|_| String::new())
                        .map_err(|e| format!("Could not move item to the Recycle Bin: {e}"))
                });
            }
            Action::NewFolder => {
                let target = unique_copy_name(&self.tabs[ti].dir.join("New folder"));
                match fs::create_dir(&target) {
                    Ok(()) => {
                        self.tabs[ti].pending_rename = Some(target);
                        self.load_tab(ti);
                    }
                    Err(e) => self.tabs[ti].error = Some(format!("Could not create folder: {e}")),
                }
            }
            Action::Properties(p) => {
                let seq = self.props_gen.fetch_add(1, AO::SeqCst) + 1;
                let mut props = build_props(&p);
                props.stats_gen = seq;
                if props.is_dir {
                    let (tx, ctx, g) = (
                        self.msg_tx.clone(),
                        self.ctx.clone(),
                        Arc::clone(&self.props_gen),
                    );
                    let root = p.clone();
                    std::thread::spawn(move || {
                        let stats = folder_stats(&root, &|| g.load(AO::Relaxed) != seq);
                        if g.load(AO::Relaxed) == seq {
                            let _ = tx.send(AppMsg::Stats(seq, stats));
                            ctx.request_repaint();
                        }
                    });
                }
                self.props = Some(props);
            }
            Action::OpenContaining(p) => {
                if let Some(parent) = p.parent().map(Path::to_path_buf) {
                    self.navigate(ti, parent);
                    self.tabs[ti].select_only(p);
                }
            }
            Action::PeaOpen(p) => {
                if let Some(exe) = &self.peazip {
                    if let Err(e) = open_with_peazip(exe, &p) {
                        self.tabs[ti].error = Some(format!("Could not start PeaZip: {e}"));
                    }
                }
            }
            Action::PeaAdd(p) => {
                if let Some(exe) = &self.peazip {
                    if let Err(e) = add_to_peazip(exe, &p) {
                        self.tabs[ti].error = Some(format!("Could not start PeaZip: {e}"));
                    }
                }
            }
            Action::ToggleHidden => {
                self.settings.show_hidden = !self.settings.show_hidden;
                self.settings_dirty = true;
                self.reload_all();
            }
            Action::OpenSettings => self.settings_open = !self.settings_open,
            Action::OpenRecycleBin => {
                if trashbin::SUPPORTED {
                    self.open_trash(ti);
                } else if let Err(e) = open_recycle_bin() {
                    self.tabs[ti].error = Some(format!("Could not open the Recycle Bin: {e}"));
                }
            }
            Action::TrashRestoreSelected => {
                let items = self.trash_items(ti, false);
                if !items.is_empty() {
                    self.run_job("Restoring…", move || {
                        trashbin::restore(items).map(|_| String::new())
                    });
                }
            }
            Action::TrashPurgeSelected => {
                let items = self.trash_items(ti, false);
                if !items.is_empty() {
                    let n = items.len();
                    let text = format!(
                        "{n} item{} will be deleted permanently. This can't be undone.",
                        if n == 1 { "" } else { "s" }
                    );
                    self.confirm = Some(ConfirmPurge { items, text });
                }
            }
            Action::TrashEmpty => {
                let items = self.trash_items(ti, true);
                if !items.is_empty() {
                    let n = items.len();
                    let text = format!("All {n} item{} in the Recycle Bin will be deleted permanently. This can't be undone.", if n == 1 { "" } else { "s" });
                    self.confirm = Some(ConfirmPurge { items, text });
                }
            }
            Action::TrashSelectAll => {
                if let Some(tr) = self.tabs[ti].trash.as_mut() {
                    tr.selected = tr.view.iter().copied().collect();
                }
            }
            Action::SelectAll => {
                let tab = &mut self.tabs[ti];
                if tab.trash.is_some() {
                    return;
                }
                tab.selected = tab
                    .view
                    .iter()
                    .map(|&i| tab.entries[i].path.clone())
                    .collect();
            }
            Action::MoveSelection(delta) => {
                let tab = &mut self.tabs[ti];
                if let Some(tr) = tab.trash.as_mut() {
                    trash_move(tr, delta);
                    return;
                }
                if tab.view.is_empty() {
                    return;
                }
                let cur = tab
                    .anchor
                    .as_ref()
                    .and_then(|s| tab.lookup.get(s))
                    .and_then(|&i| tab.view.iter().position(|&v| v == i));
                let next = match cur {
                    Some(c) => (c as i32 + delta).clamp(0, tab.view.len() as i32 - 1) as usize,
                    None => 0,
                };
                let path = tab.entries[tab.view[next]].path.clone();
                tab.select_only(path);
                tab.scroll_to = Some(next);
            }
            Action::SelectEdge(last) => {
                let tab = &mut self.tabs[ti];
                if let Some(tr) = tab.trash.as_mut() {
                    trash_edge(tr, last);
                    return;
                }
                if tab.view.is_empty() {
                    return;
                }
                let i = if last { tab.view.len() - 1 } else { 0 };
                let path = tab.entries[tab.view[i]].path.clone();
                tab.select_only(path);
                tab.scroll_to = Some(i);
            }
        }
    }
}

// ==============================================================================================
// Small widgets (all sizes come from Metrics, all colours from Palette)

fn bar_frame(pal: &Palette, l: f32, r: f32, t: f32, b: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(if pal.mica {
            Color32::TRANSPARENT
        } else {
            pal.base
        })
        .inner_margin(Margin {
            left: l as i8,
            right: r as i8,
            top: t as i8,
            bottom: b as i8,
        })
}

/// The big rounded "card" on the right (nav row, command bar, file list, status bar share it).
fn layer_frame(
    pal: &Palette,
    radius: u8,
    l: f32,
    r: f32,
    t: f32,
    b: f32,
    round_top_left: bool,
) -> egui::Frame {
    let cr = if round_top_left && pal.mica {
        CornerRadius {
            nw: radius * 2,
            ne: 0,
            sw: 0,
            se: 0,
        }
    } else {
        CornerRadius::ZERO
    };
    egui::Frame::new()
        .fill(pal.layer)
        .corner_radius(cr)
        .inner_margin(Margin {
            left: l as i8,
            right: r as i8,
            top: t as i8,
            bottom: b as i8,
        })
}

fn paint_text(ui: &Ui, rect: Rect, text: &str, font: FontId, color: Color32, right: bool) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width().max(1.0));
    let galley = ui.painter().layout_job(job);
    let x = if right {
        rect.right() - galley.size().x
    } else {
        rect.left()
    };
    let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, color);
}

fn text_width(ui: &Ui, text: &str, size: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), FontId::proportional(size), Color32::WHITE)
        .size()
        .x
}

fn draw_icon(ui: &Ui, center: egui::Pos2, size: f32, name: &str, tint: Color32) {
    egui::Image::new(icon_source(name))
        .tint(tint)
        .paint_at(ui, Rect::from_center_size(center, vec2(size, size)));
}

fn hover_fill(ui: &Ui, rect: Rect, resp: &egui::Response, pal: &Palette, m: &Metrics) {
    let c = if resp.is_pointer_button_down_on() {
        pal.subtle_pressed
    } else if resp.hovered() {
        pal.subtle_hover
    } else {
        return;
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(m.radius), c);
}

/// Hover / press feedback for list-like items (quick access, drives): accent-tinted fill plus a
/// small accent marker, so the item under the pointer is easy to spot.
fn list_hover(ui: &Ui, rect: Rect, resp: &egui::Response, pal: &Palette, m: &Metrics) {
    let down = resp.is_pointer_button_down_on();
    if !down && !resp.hovered() {
        return;
    }
    let fill = if down {
        pal.hover.gamma_multiply(1.6)
    } else {
        pal.hover
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(m.radius), fill);
    let marker = Rect::from_center_size(
        pos2(rect.left() + m.s * 2.0, rect.center().y),
        vec2(m.s * 3.0, m.font * 0.8),
    );
    ui.painter()
        .rect_filled(marker, CornerRadius::same(2), pal.accent.gamma_multiply(0.6));
}

fn icon_button(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    icon: &str,
    tip: &str,
    enabled: bool,
    toggled: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(m.ctl_h, m.ctl_h),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if toggled {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(m.radius), pal.selected);
    } else if enabled {
        hover_fill(ui, rect, &resp, pal, m);
    }
    let c = if enabled { pal.text } else { pal.text_disabled };
    draw_icon(ui, rect.center(), m.icon * 0.85, icon, c);
    resp.on_hover_text(tip)
}

fn cmd_button(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    icon: &str,
    label: Option<&str>,
    tip: &str,
    enabled: bool,
    toggled: bool,
) -> egui::Response {
    let isz = m.icon * 0.85;
    let w = match label {
        Some(l) => m.pad * 2.0 + isz + m.pad * 0.6 + text_width(ui, l, m.font),
        None => m.ctl_h,
    };
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, m.ctl_h),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if toggled {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(m.radius), pal.selected);
    } else if enabled {
        hover_fill(ui, rect, &resp, pal, m);
    }
    let c = if enabled { pal.text } else { pal.text_disabled };
    match label {
        Some(l) => {
            draw_icon(
                ui,
                pos2(rect.left() + m.pad + isz / 2.0, rect.center().y),
                isz,
                icon,
                c,
            );
            let tr =
                Rect::from_min_max(pos2(rect.left() + m.pad * 1.6 + isz, rect.top()), rect.max);
            paint_text(ui, tr, l, FontId::proportional(m.font), c, false);
        }
        None => draw_icon(ui, rect.center(), isz, icon, c),
    }
    resp.on_hover_text(tip)
}

fn vsep(ui: &mut Ui, m: &Metrics, pal: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(m.pad, m.ctl_h * 0.6), Sense::hover());
    ui.painter().line_segment(
        [pos2(r.center().x, r.top()), pos2(r.center().x, r.bottom())],
        Stroke::new(1.0_f32, pal.divider),
    );
}

fn menu_item(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    icon: Option<&str>,
    label: &str,
    hint: Option<&str>,
    enabled: bool,
) -> bool {
    let w = ui.available_width().max(m.s * 230.0);
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, m.row_h * 0.95),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if enabled && resp.hovered() {
        ui.painter().rect_filled(
            rect.shrink2(vec2(0.0, 1.0)),
            CornerRadius::same(m.radius),
            pal.control_hover,
        );
    }
    let col = if enabled { pal.text } else { pal.text_disabled };
    if let Some(i) = icon {
        draw_icon(
            ui,
            pos2(rect.left() + m.pad + m.small_icon / 2.0, rect.center().y),
            m.small_icon,
            i,
            col,
        );
    }
    let text_rect = Rect::from_min_max(
        pos2(rect.left() + m.pad * 2.2 + m.small_icon, rect.top()),
        pos2(rect.right() - m.pad, rect.bottom()),
    );
    paint_text(
        ui,
        text_rect,
        label,
        FontId::proportional(m.font),
        col,
        false,
    );
    if let Some(h) = hint {
        paint_text(
            ui,
            text_rect,
            h,
            FontId::proportional(m.font * 0.86),
            pal.text_secondary,
            true,
        );
    }
    resp.clicked()
}

fn nav_item(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    icon: &str,
    label: &str,
    selected: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), m.nav_h), Sense::click());
    let inner = rect.shrink2(vec2(0.0, 1.0));
    let r = CornerRadius::same(m.radius);
    if selected {
        ui.painter().rect_filled(inner, r, pal.control);
        if resp.hovered() {
            ui.painter().rect_filled(inner, r, pal.hover);
        }
        let pill = Rect::from_center_size(
            pos2(inner.left() + m.s * 2.0, inner.center().y),
            vec2(m.s * 3.0, m.font * 1.1),
        );
        ui.painter()
            .rect_filled(pill, CornerRadius::same(2), pal.accent);
    } else {
        list_hover(ui, inner, &resp, pal, m);
    }
    draw_icon(
        ui,
        pos2(inner.left() + m.pad * 1.5 + m.icon / 2.0, inner.center().y),
        m.icon,
        icon,
        Color32::WHITE,
    );
    let tr = Rect::from_min_max(
        pos2(inner.left() + m.pad * 2.3 + m.icon, inner.top()),
        pos2(inner.right() - m.pad, inner.bottom()),
    );
    paint_text(ui, tr, label, FontId::proportional(m.font), pal.text, false);
    resp
}

fn section_label(ui: &mut Ui, m: &Metrics, pal: &Palette, text: &str) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), m.font * 2.0), Sense::hover());
    let tr = Rect::from_min_max(pos2(rect.left() + m.pad * 1.5, rect.top()), rect.max);
    paint_text(
        ui,
        tr,
        text,
        FontId::proportional(m.font * 0.86),
        pal.text_secondary,
        false,
    );
}

fn drive_card(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    d: &DriveInfo,
    active: bool,
) -> egui::Response {
    let h = m.font * 4.4;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let inner = rect.shrink2(vec2(0.0, 1.0));
    let r = CornerRadius::same(m.radius);
    if active {
        ui.painter().rect_filled(inner, r, pal.control_hover);
        if resp.hovered() {
            ui.painter().rect_filled(inner, r, pal.hover);
        }
        let pill = Rect::from_center_size(
            pos2(inner.left() + m.s * 2.0, inner.center().y),
            vec2(m.s * 3.0, m.font * 1.6),
        );
        ui.painter()
            .rect_filled(pill, CornerRadius::same(2), pal.accent);
    } else {
        list_hover(ui, inner, &resp, pal, m);
    }
    let mount = d.mount_point.display().to_string();
    let title = if d.name.is_empty() {
        mount.clone()
    } else {
        format!("{} ({})", d.name, mount.trim_end_matches(['\\', '/']))
    };
    let x0 = inner.left() + m.pad * 1.5;
    let x1 = inner.right() - m.pad * 1.5;
    let y1 = inner.top() + h * 0.24;
    draw_icon(
        ui,
        pos2(x0 + m.icon / 2.0, y1),
        m.icon,
        "drive",
        Color32::WHITE,
    );
    paint_text(
        ui,
        Rect::from_min_max(
            pos2(x0 + m.icon + m.pad * 0.8, y1 - m.font),
            pos2(x1, y1 + m.font),
        ),
        &title,
        FontId::proportional(m.font),
        pal.text,
        false,
    );

    let used = d.total.saturating_sub(d.available);
    let frac = if d.total == 0 {
        0.0
    } else {
        (used as f32 / d.total as f32).clamp(0.0, 1.0)
    };
    let bar_h = m.s * 4.0;
    let bar = Rect::from_min_max(
        pos2(x0, inner.top() + h * 0.54),
        pos2(x1, inner.top() + h * 0.54 + bar_h),
    );
    ui.painter()
        .rect_filled(bar, CornerRadius::same(2), pal.divider);
    let color = if frac > 0.9 {
        Color32::from_rgb(0xC4, 0x2B, 0x1C)
    } else {
        pal.accent
    };
    let fill = Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar_h));
    ui.painter().rect_filled(fill, CornerRadius::same(2), color);
    let info = format!(
        "{} free of {}",
        format_size(d.available),
        format_size(d.total)
    );
    paint_text(
        ui,
        Rect::from_min_max(
            pos2(x0, inner.top() + h * 0.68),
            pos2(x1, inner.top() + h * 0.92),
        ),
        &info,
        FontId::proportional(m.font * 0.86),
        pal.text_secondary,
        false,
    );
    resp.on_hover_text(mount)
}

fn select_range(ctx: &egui::Context, id: Id, a: usize, b: usize) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(a),
            egui::text::CCursor::new(b),
        )));
    state.store(ctx, id);
}

fn crumbs(path: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for c in path.components() {
        acc.push(c.as_os_str());
        match c {
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {}
            Component::RootDir => out.push((acc.display().to_string(), acc.clone())),
            Component::Normal(n) => out.push((n.to_string_lossy().into_owned(), acc.clone())),
        }
    }
    if out.is_empty() {
        out.push((path.display().to_string(), path.to_path_buf()));
    }
    out
}

fn address_bar(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    tab: &mut Tab,
    width: f32,
    actions: &mut Vec<Action>,
) {
    let (pill, pill_resp) = ui.allocate_exact_size(vec2(width, m.ctl_h), Sense::click());
    let r = CornerRadius::same(m.radius);
    ui.painter().rect_filled(pill, r, pal.control);
    ui.painter().rect_stroke(
        pill,
        r,
        Stroke::new(1.0_f32, pal.control_stroke),
        StrokeKind::Inside,
    );
    let inner = pill.shrink2(vec2(m.pad * 0.4, 0.0));

    if tab.editing_addr {
        let id = Id::new(("addr_edit", tab.id));
        let te = egui::TextEdit::singleline(&mut tab.address)
            .id(id)
            .frame(false)
            .margin(Margin::symmetric(4, 0))
            .desired_width(inner.width())
            .vertical_align(Align::Center)
            .hint_text("Type a folder path and press Enter");
        let resp = ui.put(inner, te);
        if tab.addr_focus {
            resp.request_focus();
            select_range(ui.ctx(), id, 0, tab.address.chars().count());
            tab.addr_focus = false;
        }
        ui.painter().line_segment(
            [
                pill.left_bottom() + vec2(m.s * 3.0, -1.0),
                pill.right_bottom() + vec2(-m.s * 3.0, -1.0),
            ],
            Stroke::new(2.0 * m.s, pal.accent),
        );
        if resp.lost_focus() {
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::GoToAddress(tab.address.clone()));
            }
            tab.editing_addr = false;
            tab.address = tab.dir.display().to_string();
        }
        return;
    }

    if pill_resp.clicked() {
        tab.editing_addr = true;
        tab.addr_focus = true;
        tab.address = tab.dir.display().to_string();
    }

    let segs = crumbs(&tab.dir);
    let seg_pad = m.pad * 0.7;
    let chev = m.small_icon * 0.7;
    let sep_w = chev + m.s * 4.0;
    let more_w = m.ctl_h * 0.7;
    let widths: Vec<f32> = segs
        .iter()
        .map(|(l, _)| text_width(ui, l, m.font) + seg_pad * 2.0)
        .collect();

    // Always show the current folder; add parents from the right while they fit.
    let mut used = 0.0;
    let mut first = segs.len();
    for i in (0..segs.len()).rev() {
        let need = widths[i] + if i + 1 < segs.len() { sep_w } else { 0.0 };
        let reserve = if i > 0 { more_w + sep_w } else { 0.0 };
        if first < segs.len() && used + need + reserve > inner.width() {
            break;
        }
        used += need;
        first = i;
    }

    let top = inner.top() + m.s * 3.0;
    let h = inner.height() - m.s * 6.0;
    let mut x = inner.left();
    if first > 0 {
        let rect = Rect::from_min_size(pos2(x, top), vec2(more_w, h));
        let resp = ui.interact(rect, Id::new(("crumb_more", tab.id)), Sense::click());
        hover_fill(ui, rect, &resp, pal, m);
        paint_text(ui, rect, "…", FontId::proportional(m.font), pal.text, false);
        if resp.clicked() {
            tab.editing_addr = true;
            tab.addr_focus = true;
            tab.address = tab.dir.display().to_string();
        }
        x += more_w;
        draw_icon(
            ui,
            pos2(x + sep_w / 2.0, inner.center().y),
            chev,
            "chevron_right",
            pal.text_secondary,
        );
        x += sep_w;
    }
    for i in first..segs.len() {
        if i > first {
            draw_icon(
                ui,
                pos2(x + sep_w / 2.0, inner.center().y),
                chev,
                "chevron_right",
                pal.text_secondary,
            );
            x += sep_w;
        }
        let rect = Rect::from_min_size(
            pos2(x, top),
            vec2(widths[i].min((inner.right() - x).max(0.0)), h),
        );
        let resp = ui.interact(rect, Id::new(("crumb", tab.id, i)), Sense::click());
        hover_fill(ui, rect, &resp, pal, m);
        paint_text(
            ui,
            rect.shrink2(vec2(seg_pad, 0.0)),
            &segs[i].0,
            FontId::proportional(m.font),
            pal.text,
            false,
        );
        if resp.clicked() {
            actions.push(Action::Navigate(segs[i].1.clone()));
        }
        x += rect.width();
    }
}

fn search_box(ui: &mut Ui, m: &Metrics, pal: &Palette, tab: &mut Tab, width: f32) {
    let (pill, _) = ui.allocate_exact_size(vec2(width, m.ctl_h), Sense::hover());
    let r = CornerRadius::same(m.radius);
    ui.painter().rect_filled(pill, r, pal.control);
    ui.painter().rect_stroke(
        pill,
        r,
        Stroke::new(1.0_f32, pal.control_stroke),
        StrokeKind::Inside,
    );

    let icon_c = pos2(pill.right() - m.pad - m.small_icon / 2.0, pill.center().y);
    let te_rect = Rect::from_min_max(
        pill.min + vec2(m.pad * 0.4, 0.0),
        pill.max - vec2(m.pad * 2.0 + m.small_icon, 0.0),
    );
    let hint = format!("Search {}", tab.title());
    let te = egui::TextEdit::singleline(&mut tab.search)
        .id(Id::new("search_box"))
        .frame(false)
        .margin(Margin::symmetric(4, 0))
        .desired_width(te_rect.width())
        .vertical_align(Align::Center)
        .hint_text(hint);
    let resp = ui.put(te_rect, te);
    if resp.changed() {
        tab.view_dirty = true;
    }
    if resp.has_focus() {
        ui.painter().line_segment(
            [
                pill.left_bottom() + vec2(m.s * 3.0, -1.0),
                pill.right_bottom() + vec2(-m.s * 3.0, -1.0),
            ],
            Stroke::new(2.0 * m.s, pal.accent),
        );
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            tab.search.clear();
            tab.view_dirty = true;
            resp.surrender_focus();
        }
    }
    if tab.search.is_empty() {
        draw_icon(ui, icon_c, m.small_icon, "search", pal.text_secondary);
    } else {
        let rect = Rect::from_center_size(icon_c, vec2(m.ctl_h * 0.8, m.ctl_h * 0.8));
        let cr = ui.interact(rect, Id::new("search_clear"), Sense::click());
        hover_fill(ui, rect, &cr, pal, m);
        draw_icon(ui, icon_c, m.small_icon * 0.8, "close", pal.text);
        if cr.clicked() {
            tab.search.clear();
            tab.view_dirty = true;
        }
    }
}

fn primary_button(ui: &mut Ui, m: &Metrics, pal: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(pal.on_accent))
            .fill(pal.accent)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(m.radius))
            .min_size(vec2(m.s * 92.0, m.ctl_h)),
    )
}

fn secondary_button(ui: &mut Ui, m: &Metrics, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(text)
            .corner_radius(CornerRadius::same(m.radius))
            .min_size(vec2(m.s * 92.0, m.ctl_h)),
    )
}

/// Fluent-style dialog: rounded flyout surface, optional dimmed scrim. Returns true when closed (X / Esc).
fn dialog_shell(
    ctx: &egui::Context,
    m: &Metrics,
    pal: &Palette,
    id: &str,
    title: &str,
    modal: bool,
    width: f32,
    add: impl FnOnce(&mut Ui),
) -> bool {
    if modal {
        let screen = ctx.screen_rect();
        egui::Area::new(Id::new((id, "scrim")))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    Color32::from_black_alpha(if pal.dark { 95 } else { 48 }),
                );
            });
    }
    let mut closed = false;
    egui::Window::new(title)
        .id(Id::new(id))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(
            egui::Frame::window(&ctx.style())
                .fill(pal.flyout)
                .shadow(popup_shadow(pal.dark))
                .stroke(Stroke::new(1.0_f32, pal.divider))
                .inner_margin(Margin::same((m.pad * 2.0) as i8)),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(m.font * 1.3).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, m, pal, "close", "Close", true, false).clicked() {
                        closed = true;
                    }
                });
            });
            ui.add_space(m.pad);
            add(ui);
        });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !ctx.wants_keyboard_input() {
        closed = true;
    }
    closed
}

fn theme_pref(t: ThemeChoice) -> egui::ThemePreference {
    match t {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    }
}

// ==============================================================================================
// Custom window chrome

/// Fills the region between a right-angle corner at `apex` and a quarter circle whose centre lies at
/// `apex + (sx, sy) * r`. Used for concave "inner corner" rounding (tab wings, sidebar/file-view junction).
fn corner_fillet(
    painter: &egui::Painter,
    apex: egui::Pos2,
    r: f32,
    sx: f32,
    sy: f32,
    color: Color32,
) {
    if r < 0.5 {
        return;
    }
    let c = pos2(apex.x + sx * r, apex.y + sy * r);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(apex, color);
    const N: usize = 10;
    for i in 0..=N {
        let a = std::f32::consts::FRAC_PI_2 * i as f32 / N as f32;
        mesh.colored_vertex(pos2(c.x - sx * r * a.cos(), c.y - sy * r * a.sin()), color);
    }
    for i in 0..N {
        mesh.add_triangle(0, 1 + i as u32, 2 + i as u32);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Concave fillet at the bottom corner of a tab (Explorer-style).
fn tab_wing(ui: &Ui, corner: egui::Pos2, r: f32, left: bool, color: Color32) {
    corner_fillet(
        ui.painter(),
        corner,
        r,
        if left { -1.0 } else { 1.0 },
        -1.0,
        color,
    );
}

/// Band behind the nav/command rows: only its top-left corner can be rounded.
fn band_frame(pal: &Palette, nw: u8, l: f32, r: f32, t: f32, b: f32) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.layer)
        .corner_radius(CornerRadius {
            nw: if pal.mica { nw } else { 0 },
            ne: 0,
            sw: 0,
            se: 0,
        })
        .inner_margin(Margin {
            left: l as i8,
            right: r as i8,
            top: t as i8,
            bottom: b as i8,
        })
}

/// Minimize / maximize-restore / close, drawn with the painter so they stay crisp at any scale.
fn caption_buttons(ui: &mut Ui, m: &Metrics, pal: &Palette, area: Rect, maximized: bool) {
    let bw = area.width() / 3.0;
    let ctx = ui.ctx().clone();
    for k in 0..3usize {
        let r = Rect::from_min_size(
            pos2(area.left() + bw * k as f32, area.top()),
            vec2(bw, area.height()),
        );
        let resp = ui.interact(r, Id::new(("caption", k)), Sense::click());
        let close = k == 2;
        let bg = if resp.is_pointer_button_down_on() {
            if close {
                Color32::from_rgb(0xB3, 0x28, 0x1A)
            } else {
                pal.subtle_pressed
            }
        } else if resp.hovered() {
            if close {
                Color32::from_rgb(0xC4, 0x2B, 0x1C)
            } else {
                pal.subtle_hover
            }
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            if close && !maximized {
                ui.painter().rect_filled(
                    r,
                    egui::CornerRadius {
                        nw: 0,
                        ne: if RADIUS == 0 { RADIUS } else { RADIUS - 4 },
                        sw: 0,
                        se: 0,
                    },
                    bg,
                );
            } else {
                ui.painter().rect_filled(r, CornerRadius::ZERO, bg);
            }
        }
        let fg = if close && resp.hovered() {
            Color32::WHITE
        } else {
            pal.text
        };
        let stroke = Stroke::new(1.0_f32, fg);
        let c = r.center();
        let s = (m.s * 5.0).round();
        let p = ui.painter();
        match k {
            0 => {
                p.line_segment([pos2(c.x - s, c.y), pos2(c.x + s, c.y)], stroke);
            }
            1 if maximized => {
                let o = (m.s * 2.0).round();
                p.rect_stroke(
                    Rect::from_min_size(pos2(c.x - s, c.y - s + o), vec2(2.0 * s - o, 2.0 * s - o)),
                    CornerRadius::ZERO,
                    stroke,
                    StrokeKind::Inside,
                );
                p.line_segment([pos2(c.x - s + o, c.y - s), pos2(c.x + s, c.y - s)], stroke);
                p.line_segment([pos2(c.x + s, c.y - s), pos2(c.x + s, c.y + s - o)], stroke);
            }
            1 => {
                p.rect_stroke(
                    Rect::from_center_size(c, vec2(2.0 * s, 2.0 * s)),
                    CornerRadius::ZERO,
                    stroke,
                    StrokeKind::Inside,
                );
            }
            _ => {
                p.line_segment([pos2(c.x - s, c.y - s), pos2(c.x + s, c.y + s)], stroke);
                p.line_segment([pos2(c.x + s, c.y - s), pos2(c.x - s, c.y + s)], stroke);
            }
        }
        if resp.clicked() {
            ctx.send_viewport_cmd(match k {
                0 => egui::ViewportCommand::Minimized(true),
                1 => egui::ViewportCommand::Maximized(!maximized),
                _ => egui::ViewportCommand::Close,
            });
        }
    }
}

/// Undecorated windows have no native resize border, so handle the edges ourselves.
fn handle_resize(ctx: &egui::Context, m: &Metrics) {
    use egui::viewport::ResizeDirection as D;
    let (maximized, fullscreen) = ctx.input(|i| {
        (
            i.viewport().maximized.unwrap_or(false),
            i.viewport().fullscreen.unwrap_or(false),
        )
    });
    if maximized || fullscreen {
        return;
    }
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let r = ctx.screen_rect();
    let (b, c) = (m.s * 5.0, m.s * 12.0);
    let (el, er, et, eb) = (
        pos.x - r.left(),
        r.right() - pos.x,
        pos.y - r.top(),
        r.bottom() - pos.y,
    );
    let left = el < b || (el < c && (et < b || eb < b));
    let right = er < b || (er < c && (et < b || eb < b));
    let top = et < b || (et < c && (el < b || er < b));
    let bottom = eb < b || (eb < c && (el < b || er < b));
    let (dir, cursor) = match (left, right, top, bottom) {
        (true, _, true, _) => (D::NorthWest, egui::CursorIcon::ResizeNorthWest),
        (_, true, true, _) => (D::NorthEast, egui::CursorIcon::ResizeNorthEast),
        (true, _, _, true) => (D::SouthWest, egui::CursorIcon::ResizeSouthWest),
        (_, true, _, true) => (D::SouthEast, egui::CursorIcon::ResizeSouthEast),
        (true, ..) => (D::West, egui::CursorIcon::ResizeWest),
        (_, true, ..) => (D::East, egui::CursorIcon::ResizeEast),
        (_, _, true, _) => {
            if pos.x > r.right() - m.s * 46.0 * 3.0 {
                return; // don't steal clicks from the caption buttons
            }
            (D::North, egui::CursorIcon::ResizeNorth)
        }
        (_, _, _, true) => (D::South, egui::CursorIcon::ResizeSouth),
        _ => return,
    };
    ctx.set_cursor_icon(cursor);
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
    }
}

fn trash_move(tr: &mut TrashState, delta: i32) {
    if tr.view.is_empty() {
        return;
    }
    let cur = tr.anchor.filter(|&a| a < tr.view.len());
    let next = match cur {
        Some(c) => (c as i32 + delta).clamp(0, tr.view.len() as i32 - 1) as usize,
        None => 0,
    };
    tr.selected.clear();
    tr.selected.insert(tr.view[next]);
    tr.anchor = Some(next);
    tr.scroll_to = Some(next);
}

fn trash_edge(tr: &mut TrashState, last: bool) {
    if tr.view.is_empty() {
        return;
    }
    let i = if last { tr.view.len() - 1 } else { 0 };
    tr.selected.clear();
    tr.selected.insert(tr.view[i]);
    tr.anchor = Some(i);
    tr.scroll_to = Some(i);
}

fn rebuild_trash_view(tr: &mut TrashState, search: &str) {
    let q = search.trim().to_lowercase();
    let rows = &tr.rows;
    tr.view.clear();
    tr.view.extend((0..rows.len()).filter(|&i| {
        q.is_empty() || rows[i].name_lc.contains(&q) || rows[i].location_lc.contains(&q)
    }));
    let (col, asc) = (tr.sort_col, tr.sort_asc);
    tr.view.sort_by(|&a, &b| {
        let (ra, rb) = (&rows[a], &rows[b]);
        let o = match col {
            TrashSort::Name => natural_cmp(&ra.name_lc, &rb.name_lc),
            TrashSort::Location => ra
                .location_lc
                .cmp(&rb.location_lc)
                .then_with(|| natural_cmp(&ra.name_lc, &rb.name_lc)),
            TrashSort::Deleted => ra.deleted.cmp(&rb.deleted),
            TrashSort::Size => ra.size.cmp(&rb.size),
        };
        if asc {
            o
        } else {
            o.reverse()
        }
    });
    tr.needs_sort = false;
}

fn static_address(ui: &mut Ui, m: &Metrics, pal: &Palette, width: f32, icon: &str, label: &str) {
    let (pill, _) = ui.allocate_exact_size(vec2(width, m.ctl_h), Sense::hover());
    let r = CornerRadius::same(m.radius);
    ui.painter().rect_filled(pill, r, pal.control);
    ui.painter().rect_stroke(
        pill,
        r,
        Stroke::new(1.0_f32, pal.control_stroke),
        StrokeKind::Inside,
    );
    draw_icon(
        ui,
        pos2(pill.left() + m.pad + m.small_icon / 2.0, pill.center().y),
        m.small_icon,
        icon,
        Color32::WHITE,
    );
    let tr = Rect::from_min_max(
        pos2(pill.left() + m.pad * 1.8 + m.small_icon, pill.top()),
        pill.max,
    );
    paint_text(ui, tr, label, FontId::proportional(m.font), pal.text, false);
}

fn danger_button(ui: &mut Ui, m: &Metrics, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::WHITE))
            .fill(Color32::from_rgb(0xC4, 0x2B, 0x1C))
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(m.radius))
            .min_size(vec2(m.s * 92.0, m.ctl_h)),
    )
}

/// The Recycle Bin list: multi-select (Ctrl / Shift), sortable columns, virtualized rows.
fn trash_table_ui(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    tr: &mut TrashState,
    searching: bool,
    actions: &mut Vec<Action>,
) {
    let TrashState {
        rows,
        view,
        selected,
        anchor,
        scroll_to,
        sort_col,
        sort_asc,
        needs_sort,
        error,
        loading,
        ..
    } = tr;
    let avail = (ui.available_width() - m.pad * 0.5).max(100.0);

    if let Some(msg) = error.clone() {
        let (r, _) = ui.allocate_exact_size(vec2(avail, m.row_h * 1.1), Sense::hover());
        ui.painter()
            .rect_filled(r, CornerRadius::same(m.radius), pal.danger_bg);
        let tr_rect = Rect::from_min_max(
            r.min + vec2(m.pad, 0.0),
            r.max - vec2(m.pad * 3.0 + m.small_icon, 0.0),
        );
        paint_text(
            ui,
            tr_rect,
            &msg,
            FontId::proportional(m.font),
            pal.danger,
            false,
        );
        let cr = Rect::from_center_size(
            pos2(r.right() - m.pad - m.small_icon / 2.0, r.center().y),
            vec2(m.ctl_h * 0.8, m.ctl_h * 0.8),
        );
        let resp = ui.interact(cr, Id::new("trash_err_close"), Sense::click());
        hover_fill(ui, cr, &resp, pal, m);
        draw_icon(ui, cr.center(), m.small_icon * 0.8, "close", pal.danger);
        if resp.clicked() {
            *error = None;
        }
        ui.add_space(m.pad * 0.5);
    }

    // Columns after Name: Original location, Date deleted, Size. Dropped in that order when narrow.
    let widths = [m.s * 230.0, m.s * 150.0, m.s * 90.0];
    let mut show = [true; 3];
    let name_min = m.s * 220.0;
    let total = |show: &[bool; 3]| (0..3).filter(|&i| show[i]).map(|i| widths[i]).sum::<f32>();
    for d in [0usize, 1, 2] {
        if name_min + total(&show) > avail {
            show[d] = false;
        }
    }
    let name_w = (avail - total(&show)).max(0.0);

    let header_h = m.row_h * 0.95;
    let (hrect, _) = ui.allocate_exact_size(vec2(avail, header_h), Sense::hover());
    let labels = ["Name", "Original location", "Date deleted", "Size"];
    let sorts = [
        TrashSort::Name,
        TrashSort::Location,
        TrashSort::Deleted,
        TrashSort::Size,
    ];
    let mut x = hrect.left();
    for c in 0..4 {
        if c > 0 && !show[c - 1] {
            continue;
        }
        let cw = if c == 0 { name_w } else { widths[c - 1] };
        let crect = Rect::from_min_size(pos2(x, hrect.top()), vec2(cw, header_h));
        x += cw;
        let resp = ui.interact(crect, Id::new(("trash_hdr", c)), Sense::click());
        hover_fill(ui, crect.shrink2(vec2(m.s * 2.0, m.s * 2.0)), &resp, pal, m);
        let active = *sort_col == sorts[c];
        let color = if active { pal.text } else { pal.text_secondary };
        let right = c == 3;
        let text_left = if c == 0 {
            crect.left() + m.pad * 1.7 + m.icon
        } else {
            crect.left() + m.pad * 0.6
        };
        let tr_rect = Rect::from_min_max(
            pos2(text_left, crect.top()),
            pos2(crect.right() - m.pad * 0.6, crect.bottom()),
        );
        paint_text(
            ui,
            tr_rect,
            labels[c],
            FontId::proportional(m.font * 0.93),
            color,
            right,
        );
        if active {
            let tw = text_width(ui, labels[c], m.font * 0.93);
            let ax = if right {
                tr_rect.right() - tw - m.small_icon * 0.7
            } else {
                (text_left + tw + m.small_icon * 0.7).min(crect.right() - m.small_icon * 0.5)
            };
            draw_icon(
                ui,
                pos2(ax, crect.center().y),
                m.small_icon * 0.75,
                if *sort_asc {
                    "sort_ascending"
                } else {
                    "sort_descending"
                },
                pal.text_secondary,
            );
        }
        if resp.clicked() {
            if active {
                *sort_asc = !*sort_asc;
            } else {
                *sort_col = sorts[c];
                *sort_asc = c != 2;
            }
            *needs_sort = true;
            ui.ctx().request_repaint();
        }
    }
    ui.painter().line_segment(
        [
            pos2(hrect.left(), hrect.bottom()),
            pos2(hrect.right(), hrect.bottom()),
        ],
        Stroke::new(1.0_f32, pal.divider),
    );

    let body_rect = ui.available_rect_before_wrap();
    let body_resp = ui.interact(body_rect, Id::new("trash_body"), Sense::click());
    if body_resp.clicked() {
        selected.clear();
    }
    let any_rows = !rows.is_empty();
    body_resp.context_menu(|ui| {
        if menu_item(
            ui,
            m,
            pal,
            Some("empty_bin"),
            "Empty Recycle Bin",
            None,
            any_rows,
        ) {
            actions.push(Action::TrashEmpty);
            ui.close_menu();
        }
        if menu_item(ui, m, pal, Some("refresh"), "Refresh", Some("F5"), true) {
            actions.push(Action::Refresh);
            ui.close_menu();
        }
    });

    if view.is_empty() {
        let msg = if *loading {
            "Loading…"
        } else if searching {
            "No items match your search"
        } else {
            "The Recycle Bin is empty"
        };
        ui.painter().text(
            pos2(body_rect.center().x, body_rect.top() + m.row_h * 2.5),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(m.font),
            pal.text_secondary,
        );
        return;
    }

    let row_h = m.row_h;
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical()
        .id_salt("trash_rows")
        .drag_to_scroll(false) // dragging selects instead
        .auto_shrink([false, false])
        .show_rows(ui, row_h, view.len(), |ui, range| {
            let content_top = ui.max_rect().top() - range.start as f32 * row_h;
            let band_hit = ui.clip_rect();
            let band = rubber_band(
                ui,
                m,
                Id::new("trash_rubber"),
                content_top,
                row_h,
                view.len(),
                true,
                band_hit,
            );
            if let Some(u) = &band {
                let base_id = Id::new("trash_rubber_base");
                if u.started {
                    let base: Vec<usize> = if u.additive {
                        selected.iter().copied().collect()
                    } else {
                        Vec::new()
                    };
                    ui.ctx().data_mut(|d| d.insert_temp(base_id, base));
                }
                let base: Vec<usize> = ui.ctx().data(|d| d.get_temp(base_id)).unwrap_or_default();
                selected.clear();
                selected.extend(base);
                if let Some((lo, hi)) = u.span {
                    for vi in lo..=hi {
                        selected.insert(view[vi]);
                    }
                }
                if let Some(end) = u.end {
                    *anchor = Some(end);
                }
            }
            for vi in range.clone() {
                let ri = view[vi];
                let row = &rows[ri];
                let (rect, resp) = ui.allocate_exact_size(vec2(avail, row_h), Sense::click());
                let is_sel = selected.contains(&ri);
                let bg = if is_sel {
                    sel_fill(pal, resp.hovered())
                } else if resp.hovered() {
                    pal.hover
                } else {
                    Color32::TRANSPARENT
                };
                if bg != Color32::TRANSPARENT {
                    ui.painter().rect_filled(
                        rect.shrink2(vec2(m.s * 2.0, m.s)),
                        CornerRadius::same(m.radius),
                        bg,
                    );
                }
                if is_sel {
                    paint_sel_marker(ui, rect, m, pal);
                }
                draw_icon(
                    ui,
                    pos2(rect.left() + m.pad + m.icon / 2.0, rect.center().y),
                    m.icon,
                    row.icon,
                    Color32::WHITE,
                );
                let name_rect = Rect::from_min_max(
                    pos2(rect.left() + m.pad * 1.7 + m.icon, rect.top()),
                    pos2(rect.left() + name_w - m.pad * 0.5, rect.bottom()),
                );
                paint_text(
                    ui,
                    name_rect,
                    &row.name,
                    FontId::proportional(m.font),
                    pal.text,
                    false,
                );
                let mut cx = rect.left() + name_w;
                for ci in 0..3 {
                    if !show[ci] {
                        continue;
                    }
                    let cell = Rect::from_min_size(pos2(cx, rect.top()), vec2(widths[ci], row_h))
                        .shrink2(vec2(m.pad * 0.6, 0.0));
                    let text = match ci {
                        0 => row.location.as_str(),
                        1 => row.date_text.as_str(),
                        _ => row.size_text.as_str(),
                    };
                    paint_text(
                        ui,
                        cell,
                        text,
                        FontId::proportional(m.font),
                        pal.text_secondary,
                        ci == 2,
                    );
                    cx += widths[ci];
                }

                if resp.clicked() || resp.secondary_clicked() {
                    let mods = ui.input(|i| i.modifiers);
                    if resp.clicked() && mods.command {
                        if !selected.remove(&ri) {
                            selected.insert(ri);
                        }
                        *anchor = Some(vi);
                    } else if resp.clicked() && mods.shift && anchor.is_some() {
                        let a = anchor.unwrap_or(vi);
                        let (lo, hi) = (a.min(vi), a.max(vi));
                        selected.clear();
                        for pos in lo..=hi {
                            if let Some(&idx) = view.get(pos) {
                                selected.insert(idx);
                            }
                        }
                    } else if !(resp.secondary_clicked() && is_sel) {
                        selected.clear();
                        selected.insert(ri);
                        *anchor = Some(vi);
                    }
                }
                resp.context_menu(|ui| {
                    if menu_item(ui, m, pal, Some("restore"), "Restore", None, true) {
                        actions.push(Action::TrashRestoreSelected);
                        ui.close_menu();
                    }
                    if menu_item(
                        ui,
                        m,
                        pal,
                        Some("delete"),
                        "Delete permanently",
                        Some("Del"),
                        true,
                    ) {
                        actions.push(Action::TrashPurgeSelected);
                        ui.close_menu();
                    }
                });
            }
            if let Some(u) = &band {
                paint_band(ui, pal, u.rect);
            }
            if let Some(vi) = scroll_to.take() {
                let top = ui.max_rect().top() - range.start as f32 * row_h;
                let target = Rect::from_min_size(
                    pos2(ui.max_rect().left(), top + vi as f32 * row_h),
                    vec2(avail, row_h),
                );
                ui.scroll_to_rect(target, None);
            }
        });
}

// ==============================================================================================
// Selection visuals + drag (rubber-band) selection

/// Fill for a selected row. Deliberately stronger than the generic accent wash used elsewhere.
fn sel_fill(pal: &Palette, hovered: bool) -> Color32 {
    let base = if hovered {
        pal.selected_hover
    } else {
        pal.selected
    };
    base.gamma_multiply(1.55)
}

/// Fluent-style accent indicator on the left edge of a selected row.
fn paint_sel_marker(ui: &Ui, rect: Rect, m: &Metrics, pal: &Palette) {
    let marker = Rect::from_center_size(
        pos2(rect.left() + m.s * 4.0, rect.center().y),
        vec2(m.s * 3.0, rect.height() * 0.5),
    );
    ui.painter()
        .rect_filled(marker, CornerRadius::same(2), pal.accent);
}

#[derive(Clone, Default)]
struct Rubber {
    /// Where the button went down (screen space) - used for the drag threshold.
    press: egui::Pos2,
    /// x in screen space, y in *content* space, so the band stays anchored while the list scrolls.
    origin: egui::Pos2,
    active: bool,
    additive: bool,
}

struct BandUpdate {
    /// First frame of this drag: the caller should snapshot the selection it wants to keep.
    started: bool,
    /// Ctrl/Shift was held on press: keep the old selection and add to it.
    additive: bool,
    /// Inclusive range of list rows touched by the band.
    span: Option<(usize, usize)>,
    /// Row under the pointer (becomes the keyboard anchor).
    end: Option<usize>,
    /// Band rectangle in screen space, clipped to the list viewport.
    rect: Rect,
}

/// Drag-to-select for a virtualised, fixed-row-height list. Call it from inside the `show_rows`
/// closure (before drawing the rows) with the y of the first row of the whole list.
fn rubber_band(
    ui: &mut Ui,
    m: &Metrics,
    id: Id,
    content_top: f32,
    row_h: f32,
    rows: usize,
    enabled: bool,
    hit: Rect,
) -> Option<BandUpdate> {
    let ctx = ui.ctx().clone();
    let viewport = ui.clip_rect();
    let (pressed, down, pos, latest, dt, mods) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.interact_pos(),
            i.pointer.latest_pos(),
            i.stable_dt,
            i.modifiers,
        )
    });
    let mut state: Option<Rubber> = ctx.data_mut(|d| d.get_temp(id));

    if pressed && enabled && state.is_none() {
        if let Some(p) = pos {
            let on_scrollbar = viewport.contains(p) && p.x > viewport.right() - m.s * 14.0;
            // layer_id_at() is None over plain panels and Some(..) over dialogs/menus/popups.
            let covered = ctx.layer_id_at(p).is_some_and(|l| l != ui.layer_id());
            // `hit` is the viewport plus the blank margins around it: a drag may start there too.
            if hit.contains(p) && !on_scrollbar && !covered {
                state = Some(Rubber {
                    press: p,
                    origin: pos2(p.x, p.y - content_top),
                    active: false,
                    additive: mods.command || mods.shift,
                });
            }
        }
    }
    let mut r = state?;
    if !down {
        ctx.data_mut(|d| d.remove_temp::<Rubber>(id));
        return None;
    }
    let Some(p) = latest else {
        ctx.data_mut(|d| d.insert_temp(id, r));
        return None;
    };

    // Stay clear of egui's own click threshold (6 px) so a drag never also counts as a click.
    let mut started = false;
    if !r.active && (p - r.press).length() > 7.0 {
        r.active = true;
        started = true;
    }
    let mut out = None;
    if r.active {
        ctx.request_repaint();

        // Auto-scroll while the pointer is above/below the list.
        let dy = if p.y > viewport.bottom() {
            p.y - viewport.bottom()
        } else if p.y < viewport.top() {
            p.y - viewport.top()
        } else {
            0.0
        };
        if dy != 0.0 {
            ui.scroll_with_delta(vec2(0.0, -dy.clamp(-80.0, 80.0) * 10.0 * dt));
        }

        let cur_y = p.y - content_top;
        let (y0, y1) = (r.origin.y.min(cur_y), r.origin.y.max(cur_y));
        let span = if rows == 0 || y1 < 0.0 || y0 >= rows as f32 * row_h {
            None
        } else {
            let lo = (y0.max(0.0) / row_h).floor() as usize;
            let hi = ((y1 / row_h).floor() as usize).min(rows - 1);
            Some((lo.min(rows - 1), hi))
        };
        let end = if rows == 0 {
            None
        } else {
            Some(((cur_y / row_h).floor().max(0.0) as usize).min(rows - 1))
        };
        let origin_screen = pos2(r.origin.x, r.origin.y + content_top);
        out = Some(BandUpdate {
            started,
            additive: r.additive,
            span,
            end,
            rect: Rect::from_two_pos(origin_screen, p).intersect(viewport),
        });
    }
    ctx.data_mut(|d| d.insert_temp(id, r));
    out
}

fn paint_band(ui: &Ui, pal: &Palette, rect: Rect) {
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), pal.accent.gamma_multiply(0.18));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0_f32, pal.accent),
        StrokeKind::Inside,
    );
}

// ==============================================================================================
// The file table

#[allow(clippy::too_many_arguments)]
fn table_ui(
    ui: &mut Ui,
    m: &Metrics,
    pal: &Palette,
    tab: &mut Tab,
    cols: &mut [f32; 3],
    cut: Option<&HashSet<PathBuf>>,
    can_paste: bool,
    has_peazip: bool,
    hit: Rect,
    actions: &mut Vec<Action>,
) {
    let Tab {
        id: tab_id,
        dir,
        entries,
        view,
        selected,
        anchor,
        rename,
        scroll_to,
        sort_col,
        sort_asc,
        view_dirty,
        error,
        loading,
        search,
        ..
    } = tab;
    let tab_id = *tab_id;
    let avail = (ui.available_width() - m.pad * 0.5).max(100.0);

    // ---- error bar (Fluent InfoBar)
    if let Some(msg) = error.clone() {
        let (r, _) = ui.allocate_exact_size(vec2(avail, m.row_h * 1.1), Sense::hover());
        ui.painter()
            .rect_filled(r, CornerRadius::same(m.radius), pal.danger_bg);
        let tr = Rect::from_min_max(
            r.min + vec2(m.pad, 0.0),
            r.max - vec2(m.pad * 3.0 + m.small_icon, 0.0),
        );
        paint_text(
            ui,
            tr,
            &msg,
            FontId::proportional(m.font),
            pal.danger,
            false,
        );
        let cr = Rect::from_center_size(
            pos2(r.right() - m.pad - m.small_icon / 2.0, r.center().y),
            vec2(m.ctl_h * 0.8, m.ctl_h * 0.8),
        );
        let resp = ui.interact(cr, Id::new(("err_close", tab_id)), Sense::click());
        hover_fill(ui, cr, &resp, pal, m);
        draw_icon(ui, cr.center(), m.small_icon * 0.8, "close", pal.danger);
        if resp.clicked() {
            *error = None;
        }
        ui.add_space(m.pad * 0.5);
    }

    // ---- column geometry: Name flexes; Type, then Date, then Size are dropped when space runs out
    let base_cols = *cols;
    let w = |i: usize| base_cols[i] * m.s;
    let mut show = [true; 3];
    let name_min = m.s * 220.0;
    let total = |show: &[bool; 3]| (0..3).filter(|&i| show[i]).map(w).sum::<f32>();
    for d in [1usize, 0, 2] {
        if name_min + total(&show) > avail {
            show[d] = false;
        }
    }
    let name_w = (avail - total(&show)).max(0.0);
    let last_shown = (0..3).rev().find(|&i| show[i]);
    let first_shown = (0..3).find(|&i| show[i]);
    let mut new_cols = base_cols;

    // ---- header
    let header_h = m.row_h * 0.95;
    let (hrect, _) = ui.allocate_exact_size(vec2(avail, header_h), Sense::hover());
    let labels = ["Name", "Date modified", "Type", "Size"];
    let sorts = [
        SortColumn::Name,
        SortColumn::Modified,
        SortColumn::Type,
        SortColumn::Size,
    ];
    let mut x = hrect.left();
    for c in 0..4 {
        if c > 0 && !show[c - 1] {
            continue;
        }
        let cw = if c == 0 { name_w } else { w(c - 1) };
        let crect = Rect::from_min_size(pos2(x, hrect.top()), vec2(cw, header_h));
        x += cw;
        let resp = ui.interact(crect, Id::new(("hdr", tab_id, c)), Sense::click());
        hover_fill(ui, crect.shrink2(vec2(m.s * 2.0, m.s * 2.0)), &resp, pal, m);
        let active = *sort_col == sorts[c];
        let color = if active { pal.text } else { pal.text_secondary };
        let right = c == 3;
        let text_left = if c == 0 {
            crect.left() + m.pad * 1.7 + m.icon
        } else {
            crect.left() + m.pad * 0.6
        };
        let tr = Rect::from_min_max(
            pos2(text_left, crect.top()),
            pos2(crect.right() - m.pad * 0.6, crect.bottom()),
        );
        paint_text(
            ui,
            tr,
            labels[c],
            FontId::proportional(m.font * 0.93),
            color,
            right,
        );
        if active {
            let tw = text_width(ui, labels[c], m.font * 0.93);
            let ax = if right {
                tr.right() - tw - m.small_icon * 0.7
            } else {
                (text_left + tw + m.small_icon * 0.7).min(crect.right() - m.small_icon * 0.5)
            };
            draw_icon(
                ui,
                pos2(ax, crect.center().y),
                m.small_icon * 0.75,
                if *sort_asc {
                    "sort_ascending"
                } else {
                    "sort_descending"
                },
                pal.text_secondary,
            );
        }
        if resp.clicked() {
            if active {
                *sort_asc = !*sort_asc;
            } else {
                *sort_col = sorts[c];
                *sort_asc = true;
            }
            *view_dirty = true;
        }

        // resize handle on this column's right edge
        let target = if c == 0 {
            first_shown.map(|k| (k, -1.0))
        } else if Some(c - 1) != last_shown {
            Some((c - 1, 1.0))
        } else {
            None
        };
        if let Some((k, sign)) = target {
            let hr = Rect::from_center_size(
                pos2(crect.right(), crect.center().y),
                vec2(m.s * 8.0, header_h),
            );
            let h = ui.interact(hr, Id::new(("resize", tab_id, c)), Sense::drag());
            let hot = h.hovered() || h.dragged();
            if hot {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
            }
            if h.dragged() {
                new_cols[k] = (new_cols[k] + sign * h.drag_delta().x / m.s).clamp(50.0, 600.0);
            }
            let ly = header_h * if hot { 0.15 } else { 0.28 };
            ui.painter().line_segment(
                [
                    pos2(crect.right(), crect.top() + ly),
                    pos2(crect.right(), crect.bottom() - ly),
                ],
                Stroke::new(1.0_f32, if hot { pal.text_secondary } else { pal.divider }),
            );
        }
    }
    *cols = new_cols;
    ui.painter().line_segment(
        [
            pos2(hrect.left(), hrect.bottom()),
            pos2(hrect.right(), hrect.bottom()),
        ],
        Stroke::new(1.0_f32, pal.divider),
    );

    // ---- body
    ui.add_space(m.pad * 0.3);
    let body_rect = ui.available_rect_before_wrap();
    // The click/drag area reaches into the blank side margins, so a selection drag (or a click that
    // clears the selection) can start next to the rows instead of having to hit a row exactly.
    let band_hit = Rect::from_min_max(pos2(hit.left(), body_rect.top()), hit.max);
    let body_resp = ui.interact(band_hit, Id::new(("body", tab_id)), Sense::click());
    if body_resp.clicked() {
        selected.clear();
        *anchor = None;
    }
    body_resp.context_menu(|ui| {
        if menu_item(
            ui,
            m,
            pal,
            Some("new_folder"),
            "New folder",
            Some("Ctrl+Shift+N"),
            true,
        ) {
            actions.push(Action::NewFolder);
            ui.close_menu();
        }
        if menu_item(
            ui,
            m,
            pal,
            Some("paste"),
            "Paste",
            Some("Ctrl+V"),
            can_paste,
        ) {
            actions.push(Action::Paste);
            ui.close_menu();
        }
        if menu_item(ui, m, pal, Some("code"), "Open in Terminal", None, true) {
            actions.push(Action::OpenTerminal(dir.clone()));
            ui.close_menu();
        }
        ui.separator();
        if menu_item(ui, m, pal, Some("refresh"), "Refresh", Some("F5"), true) {
            actions.push(Action::Refresh);
            ui.close_menu();
        }
        if menu_item(ui, m, pal, Some("info"), "Properties", None, true) {
            actions.push(Action::Properties(dir.clone()));
            ui.close_menu();
        }
    });

    if view.is_empty() {
        let msg = if *loading {
            "Loading…"
        } else if !search.trim().is_empty() {
            "No items match your search"
        } else {
            "This folder is empty"
        };
        ui.painter().text(
            pos2(body_rect.center().x, body_rect.top() + m.row_h * 2.5),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(m.font),
            pal.text_secondary,
        );
        return;
    }

    let row_h = m.row_h;
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical()
        .id_salt(("rows", tab_id, dir.clone()))
        .drag_to_scroll(false) // dragging selects instead
        .auto_shrink([false, false])
        // two extra blank rows below the list: always some empty space to start a drag in
        .show_rows(ui, row_h, view.len() + 2, |ui, range| {
            let content_top = ui.max_rect().top() - range.start as f32 * row_h;
            let band = rubber_band(
                ui,
                m,
                Id::new(("rubber", tab_id)),
                content_top,
                row_h,
                view.len(),
                rename.is_none(),
                band_hit,
            );
            if let Some(u) = &band {
                let base_id = Id::new(("rubber_base", tab_id));
                if u.started {
                    let base: Vec<PathBuf> = if u.additive {
                        selected.iter().cloned().collect()
                    } else {
                        Vec::new()
                    };
                    ui.ctx().data_mut(|d| d.insert_temp(base_id, base));
                }
                let base: Vec<PathBuf> = ui.ctx().data(|d| d.get_temp(base_id)).unwrap_or_default();
                selected.clear();
                selected.extend(base);
                if let Some((lo, hi)) = u.span {
                    for vi in lo..=hi {
                        selected.insert(entries[view[vi]].path.clone());
                    }
                }
                if let Some(end) = u.end {
                    *anchor = Some(entries[view[end]].path.clone());
                }
            }
            for vi in range.clone() {
                if vi >= view.len() {
                    break; // the blank rows
                }
                let e = &entries[view[vi]];
                let (rect, resp) = ui.allocate_exact_size(vec2(avail, row_h), Sense::click());
                let is_sel = selected.contains(&e.path);
                let bg = if is_sel {
                    sel_fill(pal, resp.hovered())
                } else if resp.hovered() {
                    pal.hover
                } else {
                    Color32::TRANSPARENT
                };
                if bg != Color32::TRANSPARENT {
                    ui.painter().rect_filled(
                        rect.shrink2(vec2(m.s * 2.0, m.s)),
                        CornerRadius::same(m.radius),
                        bg,
                    );
                }
                if is_sel {
                    paint_sel_marker(ui, rect, m, pal);
                }
                let dim = cut.is_some_and(|c| c.contains(&e.path));
                let tint = if dim {
                    Color32::from_white_alpha(110)
                } else {
                    Color32::WHITE
                };
                let text_col = if dim { pal.text_disabled } else { pal.text };
                let sec_col = if dim {
                    pal.text_disabled
                } else {
                    pal.text_secondary
                };
                draw_icon(
                    ui,
                    pos2(rect.left() + m.pad + m.icon / 2.0, rect.center().y),
                    m.icon,
                    e.icon,
                    tint,
                );

                let name_rect = Rect::from_min_max(
                    pos2(rect.left() + m.pad * 1.7 + m.icon, rect.top()),
                    pos2(rect.left() + name_w - m.pad * 0.5, rect.bottom()),
                );
                let renaming_here = rename.as_ref().is_some_and(|r| r.path == e.path);
                if renaming_here {
                    let rs = rename.as_mut().unwrap();
                    let id = Id::new(("rename_edit", tab_id));
                    let er = name_rect.shrink2(vec2(0.0, m.s * 2.0));
                    let te = egui::TextEdit::singleline(&mut rs.text)
                        .id(id)
                        .margin(Margin::symmetric(4, 0))
                        .desired_width(er.width())
                        .vertical_align(Align::Center);
                    let tr = ui.put(er, te);
                    if rs.focus_pending {
                        tr.request_focus();
                        let total_chars = rs.text.chars().count();
                        let stem = if rs.is_dir {
                            total_chars
                        } else {
                            match rs.text.rfind('.') {
                                Some(i) if i > 0 => rs.text[..i].chars().count(),
                                _ => total_chars,
                            }
                        };
                        select_range(ui.ctx(), id, 0, stem);
                        rs.focus_pending = false;
                        ui.ctx().request_repaint();
                    } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        actions.push(Action::CancelRename);
                    } else if tr.lost_focus() {
                        actions.push(Action::CommitRename(rs.path.clone(), rs.text.clone()));
                    }
                } else {
                    paint_text(
                        ui,
                        name_rect,
                        &e.name,
                        FontId::proportional(m.font),
                        text_col,
                        false,
                    );
                }

                let size_text = if e.is_dir {
                    match (e.dir_size, e.is_link) {
                        (Some(s), _) => format_size(s),
                        (None, true) => "—".to_owned(),
                        (None, false) => "Calculating…".to_owned(),
                    }
                } else {
                    format_size(e.size)
                };
                let mut cx = rect.left() + name_w;
                for ci in 0..3 {
                    if !show[ci] {
                        continue;
                    }
                    let cw = w(ci);
                    let cell = Rect::from_min_size(pos2(cx, rect.top()), vec2(cw, row_h))
                        .shrink2(vec2(m.pad * 0.6, 0.0));
                    let (text, color) = match ci {
                        0 => (e.date_text.as_str(), sec_col),
                        1 => (e.type_text.as_str(), sec_col),
                        _ => (
                            size_text.as_str(),
                            if e.is_dir && e.dir_size.is_none() {
                                pal.text_disabled
                            } else {
                                sec_col
                            },
                        ),
                    };
                    paint_text(ui, cell, text, FontId::proportional(m.font), color, ci == 2);
                    cx += cw;
                }

                if (resp.clicked() || resp.secondary_clicked()) && !renaming_here {
                    let mods = ui.input(|i| i.modifiers);
                    if resp.clicked() && mods.command {
                        // Ctrl-click toggles one item.
                        if !selected.remove(&e.path) {
                            selected.insert(e.path.clone());
                        }
                        *anchor = Some(e.path.clone());
                    } else if resp.clicked() && mods.shift && anchor.is_some() {
                        // Shift-click selects the range from the anchor to this row.
                        let from = anchor
                            .as_ref()
                            .and_then(|a| view.iter().position(|&v| &entries[v].path == a))
                            .unwrap_or(vi);
                        let (lo, hi) = (from.min(vi), from.max(vi));
                        selected.clear();
                        for pos in lo..=hi {
                            selected.insert(entries[view[pos]].path.clone());
                        }
                    } else if !(resp.secondary_clicked() && is_sel) {
                        // Right-clicking inside the selection keeps it, so the menu acts on all of it.
                        selected.clear();
                        selected.insert(e.path.clone());
                        *anchor = Some(e.path.clone());
                    }
                }
                if resp.double_clicked() && !renaming_here {
                    actions.push(Action::Open(e.path.clone()));
                }
                if resp.middle_clicked() && e.is_dir {
                    actions.push(Action::NewTabAt(e.path.clone()));
                }

                resp.context_menu(|ui| {
                    let p = &e.path;
                    // Menu actions apply to the whole selection when the clicked row is part of it.
                    let targets: Vec<PathBuf> = if selected.contains(&e.path) {
                        view.iter()
                            .map(|&i| &entries[i].path)
                            .filter(|q| selected.contains(*q))
                            .cloned()
                            .collect()
                    } else {
                        vec![e.path.clone()]
                    };
                    if menu_item(ui, m, pal, Some("external"), "Open", Some("Enter"), true) {
                        actions.push(Action::Open(p.clone()));
                        ui.close_menu();
                    }
                    if e.is_dir
                        && menu_item(ui, m, pal, Some("new_tab"), "Open in new tab", None, true)
                    {
                        actions.push(Action::NewTabAt(p.clone()));
                        ui.close_menu();
                    }
                    ui.separator();
                    if menu_item(ui, m, pal, Some("cut"), "Cut", Some("Ctrl+X"), true) {
                        actions.push(Action::Cut(targets.clone()));
                        ui.close_menu();
                    }
                    if menu_item(ui, m, pal, Some("copy"), "Copy", Some("Ctrl+C"), true) {
                        actions.push(Action::Copy(targets.clone()));
                        ui.close_menu();
                    }
                    if menu_item(ui, m, pal, Some("link"), "Copy path", None, true) {
                        actions.push(Action::CopyPath(targets.clone()));
                        ui.close_menu();
                    }
                    if menu_item(
                        ui,
                        m,
                        pal,
                        Some("rename"),
                        "Rename",
                        Some("F2"),
                        targets.len() == 1,
                    ) {
                        actions.push(Action::Rename(p.clone()));
                        ui.close_menu();
                    }
                    if menu_item(ui, m, pal, Some("delete"), "Delete", Some("Del"), true) {
                        actions.push(Action::Delete(targets.clone()));
                        ui.close_menu();
                    }
                    if has_peazip {
                        ui.separator();
                        if menu_item(ui, m, pal, Some("archive"), "Open with PeaZip", None, true) {
                            actions.push(Action::PeaOpen(p.clone()));
                            ui.close_menu();
                        }
                        if menu_item(
                            ui,
                            m,
                            pal,
                            Some("archive"),
                            "Add to archive with PeaZip…",
                            None,
                            true,
                        ) {
                            actions.push(Action::PeaAdd(p.clone()));
                            ui.close_menu();
                        }
                    }
                    ui.separator();
                    if menu_item(
                        ui,
                        m,
                        pal,
                        Some("folder"),
                        "Open containing folder",
                        None,
                        true,
                    ) {
                        actions.push(Action::OpenContaining(p.clone()));
                        ui.close_menu();
                    }
                    if menu_item(
                        ui,
                        m,
                        pal,
                        Some("info"),
                        "Properties",
                        Some("Alt+Enter"),
                        true,
                    ) {
                        actions.push(Action::Properties(p.clone()));
                        ui.close_menu();
                    }
                });
            }

            if let Some(u) = &band {
                paint_band(ui, pal, u.rect);
            }

            // Keyboard navigation: bring the selected row into view even if it isn't rendered yet.
            if let Some(vi) = scroll_to.take() {
                let top = ui.max_rect().top() - range.start as f32 * row_h;
                let target = Rect::from_min_size(
                    pos2(ui.max_rect().left(), top + vi as f32 * row_h),
                    vec2(avail, row_h),
                );
                ui.scroll_to_rect(target, None);
            }
        });
}

// ==============================================================================================
// Panels
//window decoration radius for linux
const RADIUS: u8 = if cfg!(target_os = "linux") { 12 } else { 0 };

/// The contents of one pane: the file table (or the Recycle Bin) of `tab`, inside `rect`.
#[allow(clippy::too_many_arguments)]
fn tab_body(
    ui: &mut Ui,
    rect: Rect,
    m: &Metrics,
    pal: &Palette,
    tab: &mut Tab,
    cols: &mut [f32; 3],
    cut: Option<&HashSet<PathBuf>>,
    can_paste: bool,
    has_peazip: bool,
    actions: &mut Vec<Action>,
) {
    let tab_id = tab.id;
    if tab.trash.is_some() {
        let Tab {
            trash,
            search,
            view_dirty,
            ..
        } = tab;
        let tr = trash.as_mut().unwrap();
        if *view_dirty || tr.needs_sort {
            rebuild_trash_view(tr, search);
            *view_dirty = false;
        }
        let searching = !search.trim().is_empty();
        let inner = rect.shrink2(vec2(m.pad * 0.5, 0.0));
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("tab_body", tab_id))
                .max_rect(inner),
            |ui| {
                trash_table_ui(ui, m, pal, tr, searching, actions);
            },
        );
        return;
    }
    if tab.view_dirty {
        rebuild_view(tab);
    }
    // A little breathing room left and right of the list.
    let gutter = m.pad;
    let inner = Rect::from_min_max(
        pos2(rect.left() + gutter, rect.top()),
        pos2(rect.right() - gutter, rect.bottom()),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("tab_body", tab_id))
            .max_rect(inner),
        |ui| {
            table_ui(
                ui, m, pal, tab, cols, cut, can_paste, has_peazip, rect, actions,
            );
        },
    );
}

/// Lays the tile tree out inside `rect`, handling the draggable dividers on the way.
/// Collects (tab id, pane rectangle) for every leaf.
fn layout_tiles(
    ui: &mut Ui,
    t: &mut Tile,
    rect: Rect,
    m: &Metrics,
    pal: &Palette,
    out: &mut Vec<(u64, Rect)>,
) {
    match t {
        Tile::Leaf(id) => out.push((*id, rect)),
        Tile::Split {
            side_by_side,
            ratio,
            a,
            b,
        } => {
            let gap = m.s * 5.0;
            let span = (if *side_by_side {
                rect.width()
            } else {
                rect.height()
            } - gap)
                .max(1.0);
            let split = (span * *ratio).round();
            let (ra, rb, handle) = if *side_by_side {
                let x = rect.left() + split;
                (
                    Rect::from_min_max(rect.min, pos2(x, rect.bottom())),
                    Rect::from_min_max(pos2(x + gap, rect.top()), rect.max),
                    Rect::from_min_max(pos2(x, rect.top()), pos2(x + gap, rect.bottom())),
                )
            } else {
                let y = rect.top() + split;
                (
                    Rect::from_min_max(rect.min, pos2(rect.right(), y)),
                    Rect::from_min_max(pos2(rect.left(), y + gap), rect.max),
                    Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + gap)),
                )
            };
            let id = Id::new(("tile_divider", a.first_leaf(), b.first_leaf()));
            let resp = ui.interact(handle, id, Sense::drag());
            let hot = resp.hovered() || resp.dragged();
            if hot {
                ui.ctx().set_cursor_icon(if *side_by_side {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                });
            }
            if resp.dragged() {
                let d = if *side_by_side {
                    resp.drag_delta().x
                } else {
                    resp.drag_delta().y
                };
                *ratio = (*ratio + d / span).clamp(0.15, 0.85);
            }
            let c = handle.center();
            let line = if *side_by_side {
                [pos2(c.x, handle.top()), pos2(c.x, handle.bottom())]
            } else {
                [pos2(handle.left(), c.y), pos2(handle.right(), c.y)]
            };
            ui.painter().line_segment(
                line,
                Stroke::new(
                    if hot { 2.0_f32 } else { 1.0_f32 },
                    if hot { pal.accent } else { pal.divider },
                ),
            );
            layout_tiles(ui, a, ra, m, pal, out);
            layout_tiles(ui, b, rb, m, pal, out);
        }
    }
}

impl Explorer {
    /// The title bar: tabs on the left, draggable empty space, caption buttons on the right.
    fn ui_tabs(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let (m, pal) = (self.m, self.pal);
        let active = self.active;
        // 1.0 while the first tab is selected: the band below then has a square corner and the tab's wing meets the window edge.
        self.first_anim =
            ctx.animate_bool_with_time(Id::new("first_tab_active"), active == 0, 0.15);
        let info: Vec<(u64, String, String, &'static str)> = self
            .tabs
            .iter()
            .map(|t| {
                if t.trash.is_some() {
                    (t.id, t.title(), "Recycle Bin".to_owned(), "recycle_bin")
                } else {
                    (t.id, t.title(), t.dir.display().to_string(), "folder")
                }
            })
            .collect();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        // Tabs that are currently shown in a tile get a thin accent underline.
        let vis: Vec<u64> = {
            let mut v = Vec::new();
            if let Some(t) = &self.tiles {
                t.leaf_ids(&mut v);
            }
            v
        };
        let mut drag_start: Option<u64> = None;
        let bar_h = m.tab_h + m.s * 6.0;
        let caption_w = m.s * 46.0 * 3.0;
        let r_tab = m.radius + 2;
        let wing = r_tab as f32;
        let radius = if maximized { 0 } else { RADIUS };
        egui::TopBottomPanel::top("tabs")
            .show_separator_line(false)
            .exact_height(bar_h)
            .frame(
                bar_frame(&pal, 0.0, 0.0, 0.0, 0.0).corner_radius(egui::CornerRadius {
                    nw: radius,
                    ne: radius,
                    sw: 0,
                    se: 0,
                }),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let caption =
                    Rect::from_min_max(pos2(full.right() - caption_w, full.top()), full.max);

                // Empty title-bar space moves the window; double-click maximizes. Registered first so tabs sit on top.
                let drag_rect = Rect::from_min_max(full.min, pos2(caption.left(), full.bottom()));
                let drag =
                    ui.interact(drag_rect, Id::new("titlebar_drag"), Sense::click_and_drag());
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }

                // The first tab is inset by exactly one wing radius so its left wing ends at the window edge.
                let tabs_rect = Rect::from_min_max(
                    pos2(full.left() + wing, full.bottom() - m.tab_h),
                    pos2(caption.left(), full.bottom()),
                );
                let mut wings: Vec<(Rect, f32)> = Vec::new();
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(tabs_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = m.s * 2.0;
                        let n = info.len();
                        let avail = ui.available_width() - m.ctl_h - m.pad * 0.5;
                        let tab_w = (avail / n as f32).clamp(m.s * 90.0, m.s * 240.0);
                        for (i, (id, title, full_path, tab_icon)) in info.iter().enumerate() {
                            let (rect, resp) = ui
                                .allocate_exact_size(vec2(tab_w, m.tab_h), Sense::click_and_drag());
                            let is_active = i == active;
                            if resp.drag_started_by(egui::PointerButton::Primary) {
                                drag_start = Some(*id);
                            }
                            let k = ctx.animate_bool_with_time(
                                Id::new(("tab_wing", *id)),
                                is_active,
                                0.12,
                            );
                            if k > 0.01 {
                                wings.push((rect, k));
                            }
                            if is_active {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius {
                                        nw: r_tab,
                                        ne: r_tab,
                                        sw: 0,
                                        se: 0,
                                    },
                                    pal.layer,
                                );
                            } else {
                                // Resting tabs get a very faint plate; hovering strengthens it.
                                let c = if resp.hovered() {
                                    pal.control_hover
                                } else {
                                    pal.subtle_pressed
                                };
                                ui.painter().rect_filled(
                                    rect.shrink2(vec2(0.0, m.s * 3.0)),
                                    CornerRadius::same(m.radius),
                                    c,
                                );
                            }
                            if !is_active && vis.contains(id) {
                                ui.painter().rect_filled(
                                    Rect::from_min_size(
                                        pos2(rect.left() + m.pad, rect.bottom() - m.s * 4.0),
                                        vec2(rect.width() - m.pad * 2.0, m.s * 2.0),
                                    ),
                                    CornerRadius::same(1),
                                    pal.accent.gamma_multiply(0.75),
                                );
                            }
                            let close_sz = m.small_icon * 2.0;
                            let close_rect = Rect::from_center_size(
                                pos2(rect.right() - m.pad * 0.8 - close_sz / 2.0, rect.center().y),
                                vec2(close_sz, close_sz),
                            );
                            let close = ui.interact(
                                close_rect,
                                Id::new(("tab_close", *id)),
                                Sense::click(),
                            );
                            let show_close = is_active || resp.hovered() || close.hovered();
                            draw_icon(
                                ui,
                                pos2(rect.left() + m.pad + m.small_icon / 2.0, rect.center().y),
                                m.small_icon,
                                tab_icon,
                                Color32::WHITE,
                            );
                            let right_edge = if show_close {
                                close_rect.left() - m.pad * 0.3
                            } else {
                                rect.right() - m.pad
                            };
                            let tr = Rect::from_min_max(
                                pos2(rect.left() + m.pad * 1.7 + m.small_icon, rect.top()),
                                pos2(right_edge, rect.bottom()),
                            );
                            paint_text(
                                ui,
                                tr,
                                title,
                                FontId::proportional(m.font),
                                if is_active {
                                    pal.text
                                } else {
                                    pal.text_secondary
                                },
                                false,
                            );
                            if show_close {
                                if close.hovered() || close.is_pointer_button_down_on() {
                                    let c = if close.is_pointer_button_down_on() {
                                        pal.subtle_hover
                                    } else {
                                        pal.control_hover
                                    };
                                    ui.painter().rect_filled(
                                        close_rect,
                                        CornerRadius::same(m.radius),
                                        c,
                                    );
                                }
                                draw_icon(
                                    ui,
                                    close_rect.center(),
                                    m.small_icon * 0.85,
                                    "close",
                                    if close.hovered() {
                                        pal.text
                                    } else {
                                        pal.text_secondary
                                    },
                                );
                            }
                            if close.clicked() || resp.middle_clicked() {
                                actions.push(Action::CloseTab(i));
                            } else if resp.clicked() {
                                actions.push(Action::SwitchTab(i));
                            }
                            resp.on_hover_text(full_path);
                        }
                        if icon_button(ui, &m, &pal, "plus", "New tab (Ctrl+T)", true, false)
                            .clicked()
                        {
                            actions.push(Action::NewTab);
                        }
                    },
                );

                // Concave "wings" blend the selected tab into the band below it.
                for (rect, k) in &wings {
                    tab_wing(ui, rect.left_bottom(), wing * k, true, pal.layer);
                    tab_wing(ui, rect.right_bottom(), wing * k, false, pal.layer);
                }

                caption_buttons(ui, &m, &pal, caption, maximized);
            });
        if let Some(id) = drag_start {
            self.tab_drag = Some(id);
        }
    }

    fn ui_sidebar(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let (m, pal) = (self.m, self.pal);
        let cur = self.tabs[self.active].dir.clone();
        let trash_active = self.tabs[self.active].trash.is_some();
        let (quick, drives) = (&self.quick, &self.drives);
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let radius = if maximized { 0 } else { RADIUS };
        let panel = egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(m.s * 250.0)
            .width_range(m.s * 180.0..=m.s * 440.0)
            .show_separator_line(false)
            .frame(
                bar_frame(&pal, m.pad * 0.6, m.pad * 0.6, m.pad * 0.4, m.pad * 0.4).corner_radius(
                    egui::CornerRadius {
                        nw: 0,
                        ne: 0,
                        sw: radius,
                        se: 0,
                    },
                ),
            )
            .show(ctx, |ui| {
                // Bottom-up: the Recycle Bin is pinned, the rest scrolls only when it has to.
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    if nav_item(ui, &m, &pal, "recycle_bin", "Recycle Bin", trash_active).clicked()
                    {
                        actions.push(Action::OpenRecycleBin);
                    }
                    ui.add_space(m.s * 2.0);
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::ZERO, pal.divider);
                    ui.add_space(m.s * 2.0);

                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                section_label(ui, &m, &pal, "Quick access");
                                for (icon, label, path) in quick {
                                    let resp = nav_item(
                                        ui,
                                        &m,
                                        &pal,
                                        icon,
                                        label,
                                        *path == cur && !trash_active,
                                    );
                                    if resp.clicked() {
                                        actions.push(Action::Navigate(path.clone()));
                                    } else if resp.middle_clicked() {
                                        actions.push(Action::NewTabAt(path.clone()));
                                    }
                                }
                                ui.add_space(m.pad);
                                section_label(ui, &m, &pal, "Drives");
                                if drives.is_empty() {
                                    section_label(ui, &m, &pal, "No drives found");
                                }
                                for d in drives {
                                    let resp = drive_card(
                                        ui,
                                        &m,
                                        &pal,
                                        d,
                                        d.mount_point == cur && !trash_active,
                                    );
                                    if resp.clicked() {
                                        actions.push(Action::Navigate(d.mount_point.clone()));
                                    } else if resp.middle_clicked() {
                                        actions.push(Action::NewTabAt(d.mount_point.clone()));
                                    }
                                }
                            });
                    });
                });
            });
        self.sidebar_rect = Some(panel.response.rect);
    }

    fn ui_nav(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let (m, pal) = (self.m, self.pal);
        let nw = (((m.radius + 2) as f32) * (1.0 - self.first_anim)).round() as u8;
        let tab = &mut self.tabs[self.active];
        let (can_back, can_fwd, can_up) = (
            !tab.back.is_empty(),
            !tab.fwd.is_empty(),
            tab.trash.is_none() && tab.dir.parent().is_some(),
        );
        egui::TopBottomPanel::top("nav")
            .show_separator_line(false)
            .frame(band_frame(&pal, nw, m.pad, m.pad, m.pad * 0.8, m.pad * 0.2))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = m.s * 2.0;
                    if icon_button(ui, &m, &pal, "back", "Back (Alt+←)", can_back, false).clicked()
                    {
                        actions.push(Action::Back);
                    }
                    if icon_button(ui, &m, &pal, "forward", "Forward (Alt+→)", can_fwd, false)
                        .clicked()
                    {
                        actions.push(Action::Forward);
                    }
                    if icon_button(ui, &m, &pal, "up", "Up (Alt+↑)", can_up, false).clicked() {
                        actions.push(Action::Up);
                    }
                    if icon_button(ui, &m, &pal, "refresh", "Refresh (F5)", true, false).clicked() {
                        actions.push(Action::Refresh);
                    }
                    ui.add_space(m.pad * 0.5);
                    let gap = ui.spacing().item_spacing.x + m.pad * 0.5;
                    let search_w = (m.s * 260.0).min(ui.available_width() * 0.38);
                    let addr_w = (ui.available_width() - search_w - gap).max(m.s * 120.0);
                    if tab.trash.is_some() {
                        static_address(ui, &m, &pal, addr_w, "recycle_bin", "Recycle Bin");
                    } else {
                        address_bar(ui, &m, &pal, tab, addr_w, actions);
                    }
                    ui.add_space(m.pad * 0.5);
                    search_box(ui, &m, &pal, tab, search_w);
                });
            });
    }

    fn ui_command(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let (m, pal) = (self.m, self.pal);
        let (in_trash, trash_sel, trash_any) = match &self.tabs[self.active].trash {
            Some(t) => (true, t.selected.len(), !t.rows.is_empty()),
            None => (false, 0, false),
        };
        let sel = self.tabs[self.active].selected_paths();
        let (can_paste, hidden, settings_open) = (
            self.clipboard.is_some(),
            self.settings.show_hidden,
            self.settings_open,
        );
        egui::TopBottomPanel::top("command")
            .show_separator_line(false)
            .frame(layer_frame(
                &pal,
                m.radius,
                m.pad,
                m.pad,
                0.0,
                m.pad * 0.3,
                false,
            ))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = m.s * 2.0;
                    if in_trash {
                        if cmd_button(
                            ui,
                            &m,
                            &pal,
                            "restore",
                            Some("Restore"),
                            "Restore the selected items",
                            trash_sel > 0,
                            false,
                        )
                        .clicked()
                        {
                            actions.push(Action::TrashRestoreSelected);
                        }
                        if cmd_button(
                            ui,
                            &m,
                            &pal,
                            "delete",
                            Some("Delete permanently"),
                            "Delete the selected items permanently (Del)",
                            trash_sel > 0,
                            false,
                        )
                        .clicked()
                        {
                            actions.push(Action::TrashPurgeSelected);
                        }
                        vsep(ui, &m, &pal);
                        if cmd_button(
                            ui,
                            &m,
                            &pal,
                            "empty_bin",
                            Some("Empty Recycle Bin"),
                            "Permanently delete everything in the Recycle Bin",
                            trash_any,
                            false,
                        )
                        .clicked()
                        {
                            actions.push(Action::TrashEmpty);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(
                                ui,
                                &m,
                                &pal,
                                "settings",
                                "Settings",
                                true,
                                settings_open,
                            )
                            .clicked()
                            {
                                actions.push(Action::OpenSettings);
                            }
                        });
                        return;
                    }
                    let wide = ui.available_width() > m.s * 640.0;
                    if cmd_button(
                        ui,
                        &m,
                        &pal,
                        "new_folder",
                        Some("New folder"),
                        "New folder (Ctrl+Shift+N)",
                        true,
                        false,
                    )
                    .clicked()
                    {
                        actions.push(Action::NewFolder);
                    }
                    vsep(ui, &m, &pal);
                    if !sel.is_empty() {
                        if icon_button(ui, &m, &pal, "cut", "Cut (Ctrl+X)", true, false).clicked() {
                            actions.push(Action::Cut(sel.clone()));
                        }
                        if icon_button(ui, &m, &pal, "copy", "Copy (Ctrl+C)", true, false).clicked()
                        {
                            actions.push(Action::Copy(sel.clone()));
                        }
                    } else {
                        icon_button(ui, &m, &pal, "cut", "Cut (Ctrl+X)", false, false);
                        icon_button(ui, &m, &pal, "copy", "Copy (Ctrl+C)", false, false);
                    }
                    if icon_button(ui, &m, &pal, "paste", "Paste (Ctrl+V)", can_paste, false)
                        .clicked()
                    {
                        actions.push(Action::Paste);
                    }
                    if !sel.is_empty() {
                        if icon_button(ui, &m, &pal, "rename", "Rename (F2)", sel.len() == 1, false)
                            .clicked()
                        {
                            actions.push(Action::Rename(sel[0].clone()));
                        }
                        if icon_button(ui, &m, &pal, "delete", "Delete (Del)", true, false)
                            .clicked()
                        {
                            actions.push(Action::Delete(sel.clone()));
                        }
                    } else {
                        icon_button(ui, &m, &pal, "rename", "Rename (F2)", false, false);
                        icon_button(ui, &m, &pal, "delete", "Delete (Del)", false, false);
                    }
                    vsep(ui, &m, &pal);
                    if cmd_button(
                        ui,
                        &m,
                        &pal,
                        "eye",
                        if wide { Some("Hidden items") } else { None },
                        "Show hidden items",
                        true,
                        hidden,
                    )
                    .clicked()
                    {
                        actions.push(Action::ToggleHidden);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, &m, &pal, "settings", "Settings", true, settings_open)
                            .clicked()
                        {
                            actions.push(Action::OpenSettings);
                        }
                    });
                });
            });
    }

    fn ui_status(&mut self, ctx: &egui::Context) {
        let (m, pal) = (self.m, self.pal);
        let tab = &self.tabs[self.active];
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let radius = if maximized { 0 } else { RADIUS };
        let mut text = format!(
            "{} item{}",
            tab.view.len(),
            if tab.view.len() == 1 { "" } else { "s" }
        );
        if let Some(tr) = &tab.trash {
            text = format!(
                "{} item{}",
                tr.view.len(),
                if tr.view.len() == 1 { "" } else { "s" }
            );
            if !tr.selected.is_empty() {
                text.push_str(&format!("   |   {} selected", tr.selected.len()));
            }
        } else if !tab.selected.is_empty() {
            let (mut count, mut bytes, mut all_known) = (0usize, 0u64, true);
            for p in &tab.selected {
                if let Some(&i) = tab.lookup.get(p) {
                    let e = &tab.entries[i];
                    count += 1;
                    match if e.is_dir { e.dir_size } else { Some(e.size) } {
                        Some(s) => bytes = bytes.saturating_add(s),
                        None => all_known = false,
                    }
                }
            }
            if count > 0 {
                text.push_str(&format!(
                    "   |   {count} item{} selected",
                    if count == 1 { "" } else { "s" }
                ));
                if all_known {
                    text.push_str(&format!("   {}", format_size(bytes)));
                }
            }
        }
        let busy =
            self.jobs_running > 0 || tab.loading || tab.trash.as_ref().is_some_and(|t| t.loading);
        let job = self.job_label.clone();
        let clip = self.clipboard.as_ref().map(|(p, cut)| {
            let what = if p.len() == 1 {
                p[0].file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                format!("{} items", p.len())
            };
            format!("{}: {}", if *cut { "Cut" } else { "Copied" }, what)
        });
        egui::TopBottomPanel::bottom("status")
            .show_separator_line(false)
            .frame(
                layer_frame(
                    &pal,
                    m.radius,
                    m.pad * 1.5,
                    m.pad,
                    m.pad * 0.4,
                    m.pad * 0.4,
                    false,
                )
                .corner_radius(egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 0,
                    se: radius,
                }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(text)
                            .color(pal.text_secondary)
                            .size(m.font * 0.93),
                    );
                    if let Some(c) = clip {
                        ui.label(
                            RichText::new(format!("   |   {c}"))
                                .color(pal.text_secondary)
                                .size(m.font * 0.93),
                        );
                    }
                    if busy {
                        ui.add(egui::Spinner::new().size(m.font));
                        if !job.is_empty() {
                            ui.label(
                                RichText::new(job)
                                    .color(pal.text_secondary)
                                    .size(m.font * 0.93),
                            );
                        }
                    }
                });
            });
    }

    fn ui_central(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        self.fix_tiles();
        let (m, pal) = (self.m, self.pal);
        let can_paste = self.clipboard.is_some();
        let has_peazip = self.peazip.is_some();
        let cut_set: Option<HashSet<PathBuf>> = self
            .clipboard
            .as_ref()
            .filter(|c| c.1)
            .map(|c| c.0.iter().cloned().collect());
        let active = self.active;
        let mut panes: Vec<(u64, Rect)> = Vec::new();
        let mut focus: Option<usize> = None;
        let mut tree = self.tiles.take();
        let frame = layer_frame(&pal, m.radius, 0.0, 0.0, 0.0, 0.0, false);

        match tree.as_mut() {
            None => {
                let cols = &mut self.settings.col_widths;
                let tab = &mut self.tabs[active];
                let tab_id = tab.id;
                egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
                    let rect = ui.max_rect();
                    panes.push((tab_id, rect));
                    tab_body(
                        ui,
                        rect,
                        &m,
                        &pal,
                        tab,
                        cols,
                        cut_set.as_ref(),
                        can_paste,
                        has_peazip,
                        actions,
                    );
                });
            }
            Some(t) => {
                let tabs = &mut self.tabs;
                let cols = &mut self.settings.col_widths;
                egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
                    let full = ui.max_rect();
                    let mut rects: Vec<(u64, Rect)> = Vec::new();
                    layout_tiles(ui, t, full, &m, &pal, &mut rects);
                    let press = ctx.input(|i| {
                        if i.pointer.any_pressed() {
                            i.pointer.interact_pos()
                        } else {
                            None
                        }
                    });
                    let hdr_h = m.row_h;
                    for (tid, rect) in rects {
                        let Some(idx) = tabs.iter().position(|x| x.id == tid) else {
                            continue;
                        };
                        panes.push((tid, rect));
                        let is_active = idx == active;
                        // Clicking anywhere in a pane focuses it (but not through a menu or dialog on top of it).
                        if let Some(p) = press {
                            let covered = ctx.layer_id_at(p).is_some_and(|l| l != ui.layer_id());
                            if !is_active && !covered && rect.contains(p) {
                                focus = Some(idx);
                            }
                        }
                        let hdr = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + hdr_h));
                        let body = Rect::from_min_max(pos2(rect.left(), hdr.bottom()), rect.max);

                        // Pane title strip: folder name, and a button that closes just this pane.
                        ui.painter()
                            .rect_filled(hdr, CornerRadius::ZERO, pal.subtle_pressed);
                        if is_active {
                            ui.painter().rect_filled(
                                Rect::from_min_size(hdr.min, vec2(hdr.width(), m.s * 2.0)),
                                CornerRadius::ZERO,
                                pal.accent,
                            );
                        }
                        let in_bin = tabs[idx].trash.is_some();
                        draw_icon(
                            ui,
                            pos2(hdr.left() + m.pad + m.small_icon / 2.0, hdr.center().y),
                            m.small_icon,
                            if in_bin { "recycle_bin" } else { "folder" },
                            Color32::WHITE,
                        );
                        let close_rect = Rect::from_center_size(
                            pos2(hdr.right() - m.pad - m.small_icon / 2.0, hdr.center().y),
                            vec2(m.ctl_h * 0.8, m.ctl_h * 0.8),
                        );
                        let title_rect = Rect::from_min_max(
                            pos2(hdr.left() + m.pad * 1.7 + m.small_icon, hdr.top()),
                            pos2(close_rect.left() - m.pad * 0.3, hdr.bottom()),
                        );
                        paint_text(
                            ui,
                            title_rect,
                            &tabs[idx].title(),
                            FontId::proportional(m.font * 0.93),
                            if is_active {
                                pal.text
                            } else {
                                pal.text_secondary
                            },
                            false,
                        );
                        let cr = ui.interact(close_rect, Id::new(("pane_close", tid)), Sense::click());
                        hover_fill(ui, close_rect, &cr, &pal, &m);
                        draw_icon(ui, close_rect.center(), m.small_icon * 0.8, "close", pal.text_secondary);
                        cr.clone().on_hover_text("Close this pane");
                        if cr.clicked() {
                            actions.push(Action::ClosePane(tid));
                        }

                        let tab = &mut tabs[idx];
                        ui.scope_builder(
                            egui::UiBuilder::new()
                                .id_salt(("pane", tid))
                                .max_rect(body),
                            |ui| {
                                ui.set_clip_rect(body.intersect(ui.clip_rect()));
                                tab_body(
                                    ui,
                                    body,
                                    &m,
                                    &pal,
                                    tab,
                                    cols,
                                    cut_set.as_ref(),
                                    can_paste,
                                    has_peazip,
                                    actions,
                                );
                            },
                        );
                    }
                });
            }
        }
        self.tiles = tree;

        if let Some(i) = focus {
            if i != self.active && i < self.tabs.len() {
                self.active = i;
                self.on_active_changed();
                self.load_tab(i); // the pane may have been out of date: only the focused folder is watched
            }
        }
        self.tab_drag_overlay(ctx, &panes);
    }

    /// True when the OS cursor is outside this window (only known on Windows).
    fn cursor_outside_window(&self) -> Option<bool> {
        let (cx, cy) = cursor_screen_pos()?;
        let o = self.own_info?;
        Some(!(cx >= o.rect[0] && cx < o.rect[2] && cy >= o.rect[1] && cy < o.rect[3]))
    }

    /// A tab was released outside the window: open it in another SlopExplore window under the
    /// pointer, or in a new window of its own.
    fn detach_tab(&mut self, dragged: u64) {
        let Some(ti) = self.tabs.iter().position(|t| t.id == dragged) else {
            return;
        };
        if self.tabs[ti].trash.is_some() {
            return;
        }
        let Some((cx, cy)) = cursor_screen_pos() else {
            return;
        };
        let only = self.tabs.len() <= 1;
        let everything = self.ctx.input(|i| i.modifiers.shift);
        match self.peers.window_at(cx, cy) {
            Some(pid) => {
                if only || everything {
                    // Merge this whole window into the other one (the dragged tab goes last, so it
                    // ends up as the active tab there), then close this window.
                    let mut dirs: Vec<PathBuf> = self
                        .tabs
                        .iter()
                        .filter(|t| t.id != dragged && t.trash.is_none())
                        .map(|t| t.dir.clone())
                        .collect();
                    dirs.push(self.tabs[ti].dir.clone());
                    if dirs.iter().all(|d| self.peers.send_tab(pid, d)) {
                        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                } else {
                    let dir = self.tabs[ti].dir.clone();
                    if self.peers.send_tab(pid, &dir) {
                        self.apply(Action::CloseTab(ti));
                    }
                }
            }
            None => {
                if only {
                    return; // the only tab already is a window of its own
                }
                let dir = self.tabs[ti].dir.clone();
                let ppp = self.ctx.pixels_per_point().max(0.1);
                spawn_window(
                    &dir,
                    Some(((cx / ppp - 80.0).max(0.0), (cy / ppp - 20.0).max(0.0))),
                );
                self.apply(Action::CloseTab(ti));
            }
        }
    }

    /// Keeps the saved window size current and tells other windows where this one is.
    fn track_window(&mut self, ctx: &egui::Context) {
        let (inner, maxi, mini, full, focused) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.inner_rect,
                v.maximized.unwrap_or(false),
                v.minimized.unwrap_or(false),
                v.fullscreen.unwrap_or(false),
                i.focused,
            )
        });
        if !mini && !full {
            self.settings.maximized = maxi;
            if !maxi {
                if let Some(r) = inner {
                    let s = [r.width().round(), r.height().round()];
                    if s[0] >= 520.0 && s[1] >= 340.0 {
                        self.settings.window_size = Some(s);
                    }
                }
            }
        }
        if let Some(r) = inner {
            let ppp = ctx.pixels_per_point();
            let rect = [r.left() * ppp, r.top() * ppp, r.right() * ppp, r.bottom() * ppp];
            let moved = self
                .own_info
                .map_or(true, |o| o.rect.iter().zip(rect.iter()).any(|(a, b)| (a - b).abs() > 0.5));
            let refocus = focused && self.info_stamp.elapsed() > Duration::from_secs(1);
            if moved || refocus {
                let focus_ms = if focused {
                    now_ms()
                } else {
                    self.own_info.map_or(0, |o| o.focus_ms)
                };
                let info = WinInfo { rect, focus_ms };
                self.peers.set_info(info);
                self.own_info = Some(info);
                self.info_stamp = Instant::now();
            }
        }
    }

    /// While a tab is being dragged: shows where it would land (clearly, in the accent colour) and
    /// performs the drop on release.
    fn tab_drag_overlay(&mut self, ctx: &egui::Context, panes: &[(u64, Rect)]) {
        let Some(dragged) = self.tab_drag else {
            return;
        };
        let (pos, down, released) = ctx.input(|i| {
            (
                i.pointer.latest_pos(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
            )
        });
        let (m, pal) = (self.m, self.pal);
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        ctx.request_repaint();
        let outside = self.cursor_outside_window() == Some(true);
        let overlay = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            Id::new("tile_drop_preview"),
        ));
        let mut target: Option<(u64, DropZone)> = None;

        if outside {
            if self.peer_under.0.elapsed() > Duration::from_millis(150) {
                let over = cursor_screen_pos()
                    .map_or(false, |(cx, cy)| self.peers.window_at(cx, cy).is_some());
                self.peer_under = (Instant::now(), over);
            }
            let msg = if self.peer_under.1 {
                if self.tabs.len() > 1 {
                    "Release to move this tab into that window  (hold Shift: move all tabs)"
                } else {
                    "Release to merge this window into that window"
                }
            } else if self.tabs.len() > 1 {
                "Release to open this tab in its own window"
            } else {
                "Drop on another SlopExplore window to merge"
            };
            let screen = ctx.screen_rect();
            overlay.rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(110));
            let band = Rect::from_center_size(
                screen.center_bottom() - vec2(0.0, m.row_h * 2.0),
                vec2((screen.width() - m.pad * 4.0).min(m.s * 680.0), m.row_h * 1.6),
            );
            overlay.rect_filled(band, CornerRadius::same(m.radius), pal.accent);
            overlay.text(
                band.center(),
                Align2::CENTER_CENTER,
                msg,
                FontId::proportional(m.font * 1.05),
                pal.on_accent,
            );
        } else if let Some(p) = pos {
            let already = panes.iter().any(|(t, _)| *t == dragged);
            let can_split = already || panes.len() < 4;
            if let Some((tid, rect)) = panes.iter().find(|(_, r)| r.contains(p)) {
                let same = *tid == dragged;
                let zone = drop_zone(*rect, p, can_split && (!same || panes.len() == 1));
                if !(same && zone == DropZone::Center) {
                    target = Some((*tid, zone));
                }
            }
            // Dim everything, then light up the exact area the tab will occupy.
            let all = panes
                .iter()
                .fold(Rect::NOTHING, |acc, (_, r)| acc.union(*r));
            overlay.rect_filled(all, CornerRadius::ZERO, Color32::from_black_alpha(95));
            if let Some((tid, zone)) = target {
                if let Some((_, rect)) = panes.iter().find(|(t, _)| *t == tid) {
                    let zr = zone_rect(*rect, zone).shrink(m.s * 4.0);
                    overlay.rect_filled(zr, CornerRadius::same(m.radius), pal.accent.gamma_multiply(0.55));
                    overlay.rect_stroke(
                        zr,
                        CornerRadius::same(m.radius),
                        Stroke::new(3.0_f32, pal.accent),
                        StrokeKind::Inside,
                    );
                    let label = match zone {
                        DropZone::Left => "Open on the left",
                        DropZone::Right => "Open on the right",
                        DropZone::Top => "Open above",
                        DropZone::Bottom => "Open below",
                        DropZone::Center => "Replace this pane",
                    };
                    let g = overlay.layout_no_wrap(
                        label.to_owned(),
                        FontId::proportional(m.font * 1.2),
                        pal.on_accent,
                    );
                    let pill = Rect::from_center_size(zr.center(), g.size() + vec2(m.pad * 3.0, m.pad * 1.6));
                    overlay.rect_filled(pill, CornerRadius::same(m.radius), pal.accent);
                    overlay.galley(pill.center() - g.size() / 2.0, g, pal.on_accent);
                }
            }
            // A small label follows the pointer.
            let title = self
                .tabs
                .iter()
                .find(|t| t.id == dragged)
                .map(|t| t.title())
                .unwrap_or_default();
            let tip = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                Id::new("tile_drag_ghost"),
            ));
            let galley = tip.layout_no_wrap(title, FontId::proportional(m.font), pal.text);
            let r = Rect::from_min_size(p + vec2(14.0, 14.0), galley.size() + vec2(m.pad * 2.0, m.pad));
            tip.rect_filled(r, CornerRadius::same(m.radius), pal.layer);
            tip.rect_stroke(
                r,
                CornerRadius::same(m.radius),
                Stroke::new(1.0_f32, pal.accent),
                StrokeKind::Inside,
            );
            tip.galley(r.min + vec2(m.pad, m.pad * 0.5), galley, pal.text);
        }
        if released || !down {
            self.tab_drag = None;
            if let Some((tid, zone)) = target {
                self.drop_tab(dragged, tid, zone);
            } else if outside {
                self.detach_tab(dragged);
            }
        }
    }

    // ------------------------------------------------------------------ dialogs

    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let (m, pal) = (self.m, self.pal);
        let old = self.settings.clone();
        let mut s = old.clone();
        let (mut rescan, mut choose, mut delete_index) = (false, false, false);
        let mut font = self.font_edit.unwrap_or(s.font_size);
        let mut font_edit = self.font_edit;
        let index_dir = self.index_dir.display().to_string();
        let cache_len = self.indexer.len();
        let closed = dialog_shell(
            ctx,
            &m,
            &pal,
            "settings",
            "Settings",
            false,
            m.s * 440.0,
            |ui| {
                let head = |ui: &mut Ui, t: &str| {
                    ui.add_space(m.pad * 0.5);
                    ui.label(RichText::new(t).strong().color(pal.text_secondary));
                };
                head(ui, "Appearance");
                ui.horizontal(|ui| {
                    ui.label("Theme");
                    for (t, l) in [
                        (ThemeChoice::System, "System"),
                        (ThemeChoice::Light, "Light"),
                        (ThemeChoice::Dark, "Dark"),
                    ] {
                        ui.selectable_value(&mut s.theme, t, l);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Text size");
                    // The whole UI is sized from this value, so applying it mid-drag would move the slider
                    // under the cursor. Preview in the handle, apply on release.
                    let resp = ui.add(
                        egui::Slider::new(&mut font, 10.0..=24.0)
                            .step_by(0.5)
                            .suffix(" pt"),
                    );
                    if resp.dragged() {
                        font_edit = Some(font);
                    } else if resp.drag_stopped() || resp.changed() {
                        s.font_size = font;
                        font_edit = None;
                    }
                });
                ui.checkbox(
                    &mut s.follow_text_scale,
                    "Also follow Windows “Text size” (Accessibility)",
                );
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.use_system_accent, "Use Windows accent color");
                    if !s.use_system_accent {
                        ui.color_edit_button_srgb(&mut s.custom_accent);
                    }
                });
                head(ui, "Files");
                ui.checkbox(&mut s.show_hidden, "Show hidden items");
                ui.label("Date format");
                ui.horizontal_wrapped(|ui| {
                    for f in [DateFormat::European, DateFormat::Iso, DateFormat::American] {
                        ui.radio_value(&mut s.date_format, f, f.label());
                    }
                });
                head(ui, "Folder sizes");
                ui.label(
                    RichText::new(format!("Index location: {index_dir}")).color(pal.text_secondary),
                );
                ui.label(RichText::new(format!("{cache_len} folders cached. Changes are picked up automatically while the app is open.")).color(pal.text_secondary).size(m.font * 0.9));
                ui.horizontal(|ui| {
                    if secondary_button(ui, &m, "Choose location…").clicked() {
                        choose = true;
                    }
                    if secondary_button(ui, &m, "Rescan all").clicked() {
                        rescan = true;
                    }
                    if danger_button(ui, &m, "Delete index").clicked() {
                        delete_index = true;
                    }
                });
                ui.label(
                    RichText::new("Delete index removes all saved folder sizes from memory and disk; the folders on screen are measured again.")
                        .color(pal.text_secondary)
                        .size(m.font * 0.9),
                );
            },
        );

        self.font_edit = font_edit;
        let reload = s.show_hidden != old.show_hidden || s.date_format != old.date_format;
        if s.theme != old.theme {
            ctx.set_theme(theme_pref(s.theme));
        }
        self.settings = s;
        self.settings_dirty = true;
        if reload {
            self.reload_all();
        }
        if rescan {
            self.indexer.clear();
            self.start_indexing(true);
        }
        if delete_index {
            self.indexer.delete_index(&self.index_dir);
            for t in &mut self.tabs {
                for e in t.entries.iter_mut().filter(|e| e.is_dir) {
                    e.dir_size = None;
                }
            }
            self.start_indexing(true);
        }
        if choose {
            if let Some(p) = rfd::FileDialog::new()
                .set_directory(&self.index_dir)
                .pick_folder()
            {
                self.index_dir = p.clone();
                self.settings.index_dir = Some(p.clone());
                self.indexer.load(&p);
                self.start_indexing(true);
            }
        }
        if closed {
            self.font_edit = None;
            self.settings_open = false;
            save_settings(&self.settings);
            self.settings_dirty = false;
        }
    }

    fn ui_props(&mut self, ctx: &egui::Context) {
        let Some(mut p) = self.props.take() else {
            return;
        };
        let (m, pal) = (self.m, self.pal);
        let fmt = self.settings.date_format;
        let cached = self.indexer.lookup(&p.path);
        let mut readonly = p.readonly;
        let (mut copy_path, mut show_in_folder) = (false, false);
        let closed = dialog_shell(
            ctx,
            &m,
            &pal,
            "props",
            "Properties",
            false,
            m.s * 460.0,
            |ui| {
                ui.horizontal(|ui| {
                    let (r, _) =
                        ui.allocate_exact_size(vec2(m.icon * 1.8, m.icon * 1.8), Sense::hover());
                    let icon = if p.is_dir {
                        "folder"
                    } else {
                        icon_for_extension(&p.name.rsplit('.').next().unwrap_or("").to_lowercase())
                    };
                    draw_icon(ui, r.center(), m.icon * 1.8, icon, Color32::WHITE);
                    ui.add(
                        egui::Label::new(RichText::new(&p.name).size(m.font * 1.2))
                            .wrap_mode(egui::TextWrapMode::Truncate),
                    );
                });
                ui.add_space(m.pad);
                ui.separator();
                ui.add_space(m.pad * 0.5);
                egui::Grid::new("props_grid")
                    .num_columns(2)
                    .spacing(vec2(m.pad * 2.0, m.pad * 0.7))
                    .show(ui, |ui| {
                        let row = |ui: &mut Ui, k: &str, v: String| {
                            ui.label(RichText::new(k).color(pal.text_secondary));
                            ui.add(
                                egui::Label::new(v)
                                    .selectable(true)
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                            ui.end_row();
                        };
                        row(ui, "Type", p.kind.clone());
                        row(
                            ui,
                            "Location",
                            p.path
                                .parent()
                                .map(|x| x.display().to_string())
                                .unwrap_or_default(),
                        );
                        if p.is_dir {
                            match &p.stats {
                                Some(st) => {
                                    row(
                                        ui,
                                        "Size",
                                        format!(
                                            "{} ({} bytes)",
                                            format_size(st.bytes),
                                            group_digits(st.bytes)
                                        ),
                                    );
                                    row(
                                        ui,
                                        "Contains",
                                        format!(
                                            "{} files, {} folders",
                                            group_digits(st.files),
                                            group_digits(st.folders)
                                        ),
                                    );
                                }
                                None => {
                                    row(
                                        ui,
                                        "Size",
                                        cached
                                            .map(format_size)
                                            .map(|s| format!("{s} (calculating…)"))
                                            .unwrap_or_else(|| "Calculating…".into()),
                                    );
                                }
                            }
                        } else {
                            row(
                                ui,
                                "Size",
                                format!("{} ({} bytes)", format_size(p.size), group_digits(p.size)),
                            );
                        }
                        row(ui, "Created", format_date(p.created, fmt, true));
                        row(ui, "Modified", format_date(p.modified, fmt, true));
                        row(ui, "Accessed", format_date(p.accessed, fmt, true));
                    });
                ui.add_space(m.pad * 0.5);
                ui.separator();
                ui.add_space(m.pad * 0.5);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut readonly, "Read-only");
                    let mut hidden = p.hidden;
                    ui.add_enabled(false, egui::Checkbox::new(&mut hidden, "Hidden"));
                });
                ui.add_space(m.pad);
                ui.horizontal(|ui| {
                    if secondary_button(ui, &m, "Copy path").clicked() {
                        copy_path = true;
                    }
                    if secondary_button(ui, &m, "Show in folder").clicked() {
                        show_in_folder = true;
                    }
                });
            },
        );

        if readonly != p.readonly {
            if let Ok(md) = fs::metadata(&p.path) {
                let mut perm = md.permissions();
                perm.set_readonly(readonly);
                if fs::set_permissions(&p.path, perm).is_ok() {
                    p.readonly = readonly;
                }
            }
        }
        if copy_path {
            ctx.copy_text(p.path.display().to_string());
        }
        if show_in_folder {
            self.apply(Action::OpenContaining(p.path.clone()));
        }
        if closed {
            self.props_gen.fetch_add(1, AO::SeqCst); // cancels a running size scan
        } else {
            self.props = Some(p);
        }
    }

    fn ui_confirm(&mut self, ctx: &egui::Context) {
        let Some(c) = self.confirm.take() else { return };
        let (m, pal) = (self.m, self.pal);
        let mut choice = 0;
        let closed = dialog_shell(
            ctx,
            &m,
            &pal,
            "confirm",
            "Delete permanently?",
            true,
            m.s * 420.0,
            |ui| {
                ui.label(&c.text);
                ui.add_space(m.pad * 1.5);
                ui.horizontal(|ui| {
                    if danger_button(ui, &m, "Delete").clicked() {
                        choice = 1;
                    }
                    if secondary_button(ui, &m, "Cancel").clicked() {
                        choice = 2;
                    }
                });
            },
        );
        match (choice, closed) {
            (1, _) => {
                let items = c.items;
                self.run_job("Deleting permanently…", move || {
                    trashbin::purge(items).map(|_| String::new())
                });
            }
            (2, _) | (_, true) => {}
            _ => self.confirm = Some(c),
        }
    }

    /// Asks, one clashing item at a time (like Explorer), whether to replace it, keep both with a
    /// "(n)" suffix, or skip it. "Do this for all" answers the remaining ones the same way.
    fn ui_conflict(&mut self, ctx: &egui::Context) {
        let Some(mut c) = self.conflict.take() else {
            return;
        };
        let Some((src, dst)) = c.todo.front().cloned() else {
            self.run_paste(c.ready, c.cut);
            return;
        };
        let (m, pal) = (self.m, self.pal);
        let same = src == dst; // pasting a copy into the folder it came from
        let name = dst
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let taken: HashSet<PathBuf> = c
            .ready
            .iter()
            .map(|r| r.dst.clone())
            .chain(c.todo.iter().map(|(_, d)| d.clone()))
            .collect();
        let keep_name = lowest_free_name(&dst, &taken, src.is_dir())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let remaining = c.todo.len() - 1;
        let message = if same {
            format!("“{name}” is already in this folder.")
        } else {
            format!("A file or folder named “{name}” already exists here.")
        };
        let mut all = c.apply_all;
        let mut choice = 0;
        let closed = dialog_shell(
            ctx,
            &m,
            &pal,
            "conflict",
            "Item already exists",
            true,
            m.s * 460.0,
            |ui| {
                ui.label(&message);
                ui.add_space(m.pad * 0.5);
                ui.label(
                    RichText::new(format!("Keep both saves the new item as “{keep_name}”."))
                        .color(pal.text_secondary),
                );
                if remaining > 0 {
                    ui.add_space(m.pad * 0.5);
                    ui.checkbox(
                        &mut all,
                        format!(
                            "Do this for the {remaining} remaining conflict{}",
                            if remaining == 1 { "" } else { "s" }
                        ),
                    );
                }
                ui.add_space(m.pad * 1.5);
                ui.horizontal(|ui| {
                    if same {
                        if primary_button(ui, &m, &pal, "Keep both").clicked() {
                            choice = 2;
                        }
                    } else {
                        if primary_button(ui, &m, &pal, "Replace").clicked() {
                            choice = 1;
                        }
                        if secondary_button(ui, &m, "Keep both").clicked() {
                            choice = 2;
                        }
                    }
                    if secondary_button(ui, &m, "Skip").clicked() {
                        choice = 3;
                    }
                    if secondary_button(ui, &m, "Cancel").clicked() {
                        choice = 4;
                    }
                });
            },
        );
        match (choice, closed) {
            (4, _) | (_, true) => {} // cancel the whole paste
            (0, false) => {
                c.apply_all = all;
                self.conflict = Some(c);
            }
            (ch, _) => {
                let n = if all { c.todo.len() } else { 1 };
                for _ in 0..n {
                    let Some((src, dst)) = c.todo.pop_front() else {
                        break;
                    };
                    match ch {
                        1 if src != dst => c.ready.push(PasteItem {
                            src,
                            dst,
                            replace: true,
                        }),
                        1 | 2 => {
                            let taken: HashSet<PathBuf> = c
                                .ready
                                .iter()
                                .map(|r| r.dst.clone())
                                .chain(c.todo.iter().map(|(_, d)| d.clone()))
                                .collect();
                            let new = lowest_free_name(&dst, &taken, src.is_dir());
                            c.ready.push(PasteItem {
                                src,
                                dst: new,
                                replace: false,
                            });
                        }
                        _ => {} // skip this one
                    }
                }
                if c.todo.is_empty() {
                    self.run_paste(c.ready, c.cut);
                } else {
                    c.apply_all = all;
                    self.conflict = Some(c);
                }
            }
        }
    }
}

// ==============================================================================================

impl eframe::App for Explorer {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // every panel paints its own background; Mica shows through the transparent parts
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.sync_system(ctx, frame);
        self.track_window(ctx);
        handle_resize(ctx, &self.m);
        self.pump_messages();
        let rect = ctx.screen_rect();
        let painter = ctx.layer_painter(egui::LayerId::background());

        if self.mica{
            painter.rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(120, 120, 120, 10));
        }else {
            painter.rect_filled(rect, 8.0, self.pal.layer);
        }
        let mut actions: Vec<Action> = Vec::new();
        self.collect_shortcuts(ctx, &mut actions);

        self.ui_tabs(ctx, &mut actions);
        self.ui_nav(ctx, &mut actions);
        self.ui_command(ctx, &mut actions);
        self.ui_sidebar(ctx, &mut actions);
        self.ui_status(ctx);
        self.ui_central(ctx, &mut actions);
        // Rounded inner corner where the band above meets the file view, to the right of the sidebar.
        if let Some(r) = self.sidebar_rect {
            let painter = ctx.layer_painter(egui::LayerId::background());
            corner_fillet(
                &painter,
                pos2(r.right(), r.top()),
                self.m.radius as f32 * 2.0,
                -1.0,
                1.0,
                self.pal.layer,
            );
        }
        self.ui_settings(ctx);
        self.ui_props(ctx);
        self.ui_conflict(ctx);
        self.ui_confirm(ctx);

        for a in actions {
            self.apply(a);
        }

        let title = format!("{} – SlopExplorer", self.tabs[self.active].title());
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            // Vanish first, then save: the window is gone the instant the button is pressed.
            hide_window(frame);
            save_settings(&self.settings);
            self.peers.shutdown();
            self.indexer.save_if_dirty(&self.index_dir, true);
            // Skip the normal teardown (joining threads, destroying the GL context, DLL detach in
            // GPU drivers): it is slow and can hang a transparent window. The OS cleans up at once.
            terminate_now();
        }
    }
}

/// Folder a window opened by "drag a tab out" starts in.
static START_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Starts another SlopExplore window (its own process) showing `dir`.
fn spawn_window(dir: &Path, pos: Option<(f32, f32)>) {
    if let Ok(exe) = std::env::current_exe() {
        let mut c = std::process::Command::new(exe);
        c.arg("--open").arg(dir);
        if let Some((x, y)) = pos {
            c.arg("--pos").arg(format!("{x:.0},{y:.0}"));
        }
        let _ = c.spawn();
    }
}

fn main() -> eframe::Result<()> {
    // The release build has no console, so a panic would otherwise vanish without a trace.
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let dir = app_dir();
        let _ = fs::create_dir_all(&dir);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            let _ = writeln!(
                f,
                "---- panic ----\n{info}\n{}\n",
                std::backtrace::Backtrace::force_capture()
            );
        }
    }));

    // Command line (used when a tab is dragged out into a window of its own).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos: Option<[f32; 2]> = None;
    let mut k = 0;
    while k < args.len() {
        match args[k].as_str() {
            "--open" if k + 1 < args.len() => {
                let _ = START_DIR.set(PathBuf::from(&args[k + 1]));
                k += 1;
            }
            "--pos" if k + 1 < args.len() => {
                if let Some((x, y)) = args[k + 1].split_once(',') {
                    if let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>()) {
                        pos = Some([x, y]);
                    }
                }
                k += 1;
            }
            _ => {}
        }
        k += 1;
    }

    // Assumes main.rs is in src/ and logo.ico is in assets/.
    let rgba = image::load_from_memory(include_bytes!("../assets/logo.ico"))
        .expect("Could not load assets/logo.ico")
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    let icon = egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    };

    // Same size as last time.
    let saved = load_settings();
    let size = saved
        .window_size
        .filter(|s| s[0] >= 520.0 && s[1] >= 340.0 && s[0] <= 10000.0 && s[1] <= 10000.0)
        .unwrap_or([1180.0, 760.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size([520.0, 340.0])
        .with_resizable(true)
        .with_decorations(false) // we draw the title bar and caption buttons ourselves
        .with_transparent(true) // required for the Mica backdrop
        .with_icon(Arc::new(icon));
    if let Some(p) = pos {
        viewport = viewport.with_position(p);
    } else if saved.maximized {
        viewport = viewport.with_maximized(true);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "SlopExplore",
        options,
        Box::new(|cc| Ok(Box::new(Explorer::new(cc)))),
    )
}
