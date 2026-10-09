//! Talks to the other SlopExplore windows. Every window is its own process; they find each other
//! through small files in the app folder:
//!   windows/<pid>.txt   - the window's screen rectangle (refreshed every couple of seconds)
//!   inbox/<pid>/*.txt   - folders other windows have handed over (one path per file)

use eframe::egui;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where a window is on screen, in physical pixels, and when it last had the focus.
#[derive(Clone, Copy)]
pub struct WinInfo {
    pub rect: [f32; 4], // left, top, right, bottom
    pub focus_ms: u64,
}

pub struct Peers {
    base: PathBuf,
    pid: u32,
    info: Arc<Mutex<Option<WinInfo>>>,
    inbox: Arc<Mutex<Vec<PathBuf>>>,
    stop: Arc<AtomicBool>,
}

impl Peers {
    pub fn new(ctx: egui::Context, base: &Path) -> Self {
        let me = Self {
            base: base.to_path_buf(),
            pid: std::process::id(),
            info: Arc::new(Mutex::new(None)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let (info, inbox, stop) = (
            Arc::clone(&me.info),
            Arc::clone(&me.inbox),
            Arc::clone(&me.stop),
        );
        let (base, pid) = (me.base.clone(), me.pid);
        std::thread::spawn(move || {
            let reg_dir = base.join("windows");
            let mine = base.join("inbox").join(pid.to_string());
            let _ = fs::create_dir_all(&reg_dir);
            let _ = fs::create_dir_all(&mine);
            let reg_file = reg_dir.join(format!("{pid}.txt"));
            let mut last: Option<WinInfo> = None;
            let mut last_write = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // Publish our rectangle when it changed, and as a heartbeat every two seconds.
                let cur = info.lock().ok().and_then(|g| *g);
                if let Some(c) = cur {
                    let now = now_ms();
                    let changed = last.map_or(true, |l| l.rect != c.rect || l.focus_ms != c.focus_ms);
                    if changed || now.saturating_sub(last_write) > 2000 {
                        let text = format!(
                            "{} {} {} {} {} {}",
                            c.rect[0], c.rect[1], c.rect[2], c.rect[3], c.focus_ms, now
                        );
                        let _ = fs::write(&reg_file, text);
                        last = Some(c);
                        last_write = now;
                    }
                }
                // Pick up tabs other windows have handed to us.
                let mut got = false;
                if let Ok(rd) = fs::read_dir(&mine) {
                    // File names start with a timestamp and a counter: sorting keeps the sending order.
                    let mut files: Vec<PathBuf> = rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("txt"))
                        .collect();
                    files.sort();
                    for p in files {
                        if let Ok(text) = fs::read_to_string(&p) {
                            if let Some(line) = text.lines().next() {
                                if let Ok(mut v) = inbox.lock() {
                                    v.push(PathBuf::from(line.trim()));
                                    got = true;
                                }
                            }
                        }
                        let _ = fs::remove_file(&p);
                    }
                }
                if got {
                    ctx.request_repaint();
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        me
    }

    pub fn set_info(&self, info: WinInfo) {
        if let Ok(mut g) = self.info.lock() {
            *g = Some(info);
        }
    }

    pub fn take_inbox(&self) -> Vec<PathBuf> {
        self.inbox
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }

    /// The other SlopExplore window (if any) under the given screen point; the most recently
    /// focused one wins when windows overlap.
    pub fn window_at(&self, x: f32, y: f32) -> Option<u32> {
        let now = now_ms() as f64;
        let (x, y) = (x as f64, y as f64);
        let mut best: Option<(u64, u32)> = None;
        for e in fs::read_dir(self.base.join("windows")).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(pid) = name
                .strip_suffix(".txt")
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == self.pid {
                continue;
            }
            let Ok(text) = fs::read_to_string(e.path()) else {
                continue;
            };
            let v: Vec<f64> = text
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if v.len() < 6 {
                continue;
            }
            let age = now - v[5];
            if age > 60_000.0 {
                let _ = fs::remove_file(e.path()); // left behind by a crashed window
                continue;
            }
            if age > 6_000.0 {
                continue;
            }
            if x >= v[0] && x < v[2] && y >= v[1] && y < v[3] {
                let focus = v[4] as u64;
                if best.map_or(true, |(f, _)| focus >= f) {
                    best = Some((focus, pid));
                }
            }
        }
        best.map(|b| b.1)
    }

    /// Hands a folder to another window; it opens it as a new tab.
    pub fn send_tab(&self, pid: u32, dir: &Path) -> bool {
        let d = self.base.join("inbox").join(pid.to_string());
        if fs::create_dir_all(&d).is_err() {
            return false;
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let stem = format!(
            "{:013}_{:06}_{}",
            now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed) % 1_000_000,
            self.pid
        );
        let tmp = d.join(format!("{stem}.tmp"));
        if fs::write(&tmp, dir.display().to_string()).is_err() {
            return false;
        }
        fs::rename(&tmp, d.join(format!("{stem}.txt"))).is_ok()
    }

    /// Removes our traces (the process is about to be terminated without running destructors).
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = fs::remove_file(self.base.join("windows").join(format!("{}.txt", self.pid)));
        let _ = fs::remove_dir_all(self.base.join("inbox").join(self.pid.to_string()));
    }
}

/// The mouse pointer in physical screen pixels (works while it is outside our window).
#[cfg(windows)]
pub fn cursor_screen_pos() -> Option<(f32, f32)> {
    #[repr(C)]
    struct Pt {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(p: *mut Pt) -> i32;
    }
    let mut p = Pt { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x as f32, p.y as f32))
    } else {
        None
    }
}
#[cfg(not(windows))]
pub fn cursor_screen_pos() -> Option<(f32, f32)> {
    None
}
