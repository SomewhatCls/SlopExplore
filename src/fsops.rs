//! File-system helpers and formatting that don't touch the UI.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DateFormat {
    #[default]
    European, // 31.12.2025 14:05
    Iso,      // 2025-12-31 14:05
    American, // 12/31/2025 2:05 PM
}

impl DateFormat {
    pub fn label(self) -> &'static str {
        match self {
            DateFormat::European => "31.12.2025 14:05",
            DateFormat::Iso => "2025-12-31 14:05",
            DateFormat::American => "12/31/2025 2:05 PM",
        }
    }
    fn pattern(self, seconds: bool) -> &'static str {
        match (self, seconds) {
            (DateFormat::European, false) => "%d.%m.%Y %H:%M",
            (DateFormat::European, true) => "%d.%m.%Y %H:%M:%S",
            (DateFormat::Iso, false) => "%Y-%m-%d %H:%M",
            (DateFormat::Iso, true) => "%Y-%m-%d %H:%M:%S",
            (DateFormat::American, false) => "%m/%d/%Y %-I:%M %p",
            (DateFormat::American, true) => "%m/%d/%Y %-I:%M:%S %p",
        }
    }
}

pub fn format_date(time: Option<SystemTime>, fmt: DateFormat, seconds: bool) -> String {
    match time {
        Some(t) => DateTime::<Local>::from(t).format(fmt.pattern(seconds)).to_string(),
        None => "—".to_owned(),
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{size:.1} {}", UNITS[unit]) }
}

pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "report10" sorts after "report2".
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

#[cfg(windows)]
pub fn attributes(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}
#[cfg(not(windows))]
pub fn attributes(_md: &fs::Metadata) -> u32 {
    0
}

/// Uses the attributes already delivered by `read_dir` (no extra syscall per entry, and correct for
/// junctions such as "Application Data", which `fs::metadata` can't follow).
pub fn is_hidden_entry(name: &str, md: &fs::Metadata) -> bool {
    attributes(md) & (0x2 | 0x4) != 0 || name.starts_with('.')
}

pub fn remove_existing(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

pub fn unique_copy_name(destination: &Path) -> PathBuf {
    if !destination.exists() {
        return destination.to_path_buf();
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let stem = destination.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Copy".into());
    let ext = destination.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 1u64.. {
        let name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

pub fn copy_item(source: &Path, destination: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        if destination.starts_with(source) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "cannot copy a folder into itself"));
        }
        fs::create_dir_all(destination)?;
        for item in fs::read_dir(source)? {
            let item = item?;
            copy_item(&item.path(), &destination.join(item.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(source, destination).map(|_| ())
    }
}

/// Rename when possible, otherwise copy + delete (moves across drives).
pub fn move_item(source: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_item(source, destination)?;
            remove_existing(source)
        }
    }
}

pub fn open_recycle_bin() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        return std::process::Command::new("explorer.exe").arg("shell:RecycleBinFolder").spawn().map(|_| ());
    }
    #[cfg(target_os = "macos")]
    {
        let trash = dirs::home_dir()
            .map(|h| h.join(".Trash"))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "home directory not found"))?;
        return open::that(trash).map_err(std::io::Error::other);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return open::that("trash:///").map_err(std::io::Error::other);
    }
    #[allow(unreachable_code)]
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "Recycle Bin is not available here"))
}

pub fn find_peazip() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(p) = std::env::var_os(var) {
                let c = PathBuf::from(p).join("PeaZip").join("peazip.exe");
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("PATH")
            .and_then(|paths| std::env::split_paths(&paths).map(|d| d.join("peazip")).find(|c| c.is_file()))
    }
}

pub fn open_with_peazip(exe: &Path, path: &Path) -> std::io::Result<()> {
    let mut c = std::process::Command::new(exe);
    c.arg(if path.is_dir() { "-ext2browsepath" } else { "-ext2browse" });
    c.arg(path).spawn().map(|_| ())
}

pub fn add_to_peazip(exe: &Path, path: &Path) -> std::io::Result<()> {
    std::process::Command::new(exe).arg("-add2archive").arg(path).spawn().map(|_| ())
}

#[derive(Default, Clone)]
pub struct FolderStats {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// Iterative walk (no recursion depth issues); `cancelled` lets the Properties dialog abort it.
pub fn folder_stats(root: &Path, cancelled: &dyn Fn() -> bool) -> FolderStats {
    let mut stats = FolderStats::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancelled() {
            break;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for item in rd.flatten() {
            let Ok(ft) = item.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stats.folders += 1;
                stack.push(item.path());
            } else {
                stats.files += 1;
                stats.bytes += item.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    stats
}
