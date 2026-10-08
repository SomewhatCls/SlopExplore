//! Native Recycle Bin access (Windows + freedesktop Linux) through the `trash` crate.
//! On other platforms the stubs report "not supported" and the app falls back to the system file manager.

use crate::fsops::{format_date, format_size, DateFormat};
use crate::icons::icon_for_extension;
use std::time::SystemTime;

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
mod imp {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    pub type Item = trash::TrashItem;
    pub const SUPPORTED: bool = true;

    pub fn load(fmt: DateFormat) -> Result<Vec<TrashRow>, String> {
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let (size, is_dir) = match trash::os_limited::metadata(&item) {
                Ok(md) => match md.size {
                    trash::TrashItemSize::Bytes(b) => (Some(b), false),
                    trash::TrashItemSize::Entries(n) => (Some(n as u64), true),
                },
                Err(_) => (None, false),
            };
            let original = item.original_path();
            let name = original.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let location = original.parent().map(|p| p.display().to_string()).unwrap_or_default();
            let deleted = (item.time_deleted >= 0).then(|| UNIX_EPOCH + Duration::from_secs(item.time_deleted as u64));
            out.push(make_row(item, name, location, deleted, size, is_dir, fmt));
        }
        Ok(out)
    }

    pub fn restore(items: Vec<Item>) -> Result<(), String> {
        trash::os_limited::restore_all(items).map_err(|e| e.to_string())
    }

    pub fn purge(items: Vec<Item>) -> Result<(), String> {
        trash::os_limited::purge_all(items).map_err(|e| e.to_string())
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
mod imp {
    use super::*;

    #[derive(Clone)]
    pub struct Item;
    pub const SUPPORTED: bool = false;

    pub fn load(_fmt: DateFormat) -> Result<Vec<TrashRow>, String> {
        Err("The Recycle Bin view is not supported on this platform.".into())
    }
    pub fn restore(_items: Vec<Item>) -> Result<(), String> {
        Err("Not supported on this platform.".into())
    }
    pub fn purge(_items: Vec<Item>) -> Result<(), String> {
        Err("Not supported on this platform.".into())
    }
}

pub use imp::*;

pub struct TrashRow {
    pub item: Item,
    pub name: String,
    pub name_lc: String,
    pub location: String,
    pub location_lc: String,
    pub deleted: Option<SystemTime>,
    pub date_text: String,
    pub size: Option<u64>,
    pub size_text: String,
    pub icon: &'static str,
}

#[allow(dead_code)]
fn make_row(item: Item, name: String, location: String, deleted: Option<SystemTime>, size: Option<u64>, is_dir: bool, fmt: DateFormat) -> TrashRow {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    TrashRow {
        item,
        name_lc: name.to_lowercase(),
        location_lc: location.to_lowercase(),
        date_text: format_date(deleted, fmt, false),
        size_text: match size {
            Some(n) if is_dir => format!("{n} item{}", if n == 1 { "" } else { "s" }),
            Some(b) => format_size(b),
            None => "—".to_owned(),
        },
        icon: if is_dir { "folder" } else { icon_for_extension(&ext) },
        name,
        location,
        deleted,
        size,
    }
}
