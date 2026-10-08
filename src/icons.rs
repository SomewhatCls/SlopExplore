use eframe::egui;

/// Looks up an embedded SVG by name. Unknown names fall back to the generic file icon.
pub fn icon_source(name: &str) -> egui::ImageSource<'static> {
    match name {
        "app" => egui::include_image!("../assets/app.svg"),
        "archive" => egui::include_image!("../assets/archive.svg"),
        "audio" => egui::include_image!("../assets/audio.svg"),
        "back" => egui::include_image!("../assets/back.svg"),
        "check" => egui::include_image!("../assets/check.svg"),
        "chevron_down" => egui::include_image!("../assets/chevron_down.svg"),
        "chevron_right" => egui::include_image!("../assets/chevron_right.svg"),
        "close" => egui::include_image!("../assets/close.svg"),
        "code" => egui::include_image!("../assets/code.svg"),
        "copy" => egui::include_image!("../assets/copy.svg"),
        "cut" => egui::include_image!("../assets/cut.svg"),
        "delete" => egui::include_image!("../assets/delete.svg"),
        "desktop" => egui::include_image!("../assets/desktop.svg"),
        "document" => egui::include_image!("../assets/document.svg"),
        "documents" => egui::include_image!("../assets/documents.svg"),
        "downloads" => egui::include_image!("../assets/downloads.svg"),
        "drive" => egui::include_image!("../assets/drive.svg"),
        "external" => egui::include_image!("../assets/external.svg"),
        "eye" => egui::include_image!("../assets/eye.svg"),
        "file" => egui::include_image!("../assets/file.svg"),
        "folder" => egui::include_image!("../assets/folder.svg"),
        "forward" => egui::include_image!("../assets/forward.svg"),
        "home" => egui::include_image!("../assets/home.svg"),
        "image" => egui::include_image!("../assets/image.svg"),
        "info" => egui::include_image!("../assets/info.svg"),
        "link" => egui::include_image!("../assets/link.svg"),
        "music" => egui::include_image!("../assets/music.svg"),
        "new_folder" => egui::include_image!("../assets/new_folder.svg"),
        "new_tab" => egui::include_image!("../assets/new_tab.svg"),
        "paste" => egui::include_image!("../assets/paste.svg"),
        "pictures" => egui::include_image!("../assets/pictures.svg"),
        "plus" => egui::include_image!("../assets/plus.svg"),
        "recycle_bin" => egui::include_image!("../assets/recycle_bin.svg"),
        "refresh" => egui::include_image!("../assets/refresh.svg"),
        "rename" => egui::include_image!("../assets/rename.svg"),
        "search" => egui::include_image!("../assets/search.svg"),
        "settings" => egui::include_image!("../assets/settings.svg"),
        "sort_ascending" => egui::include_image!("../assets/sort_ascending.svg"),
        "sort_descending" => egui::include_image!("../assets/sort_descending.svg"),
        "up" => egui::include_image!("../assets/up.svg"),
        "video" => egui::include_image!("../assets/video.svg"),
        "videos" => egui::include_image!("../assets/videos.svg"),
        "restore" => egui::include_image!("../assets/restore.svg"),
        "empty_bin" => egui::include_image!("../assets/empty_bin.svg"),
        _ => egui::include_image!("../assets/file.svg"),
    }
}

/// Picks a file-type icon from a lower-case extension.
pub fn icon_for_extension(ext: &str) -> &'static str {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff" | "heic" | "avif" => "image",
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" => "audio",
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" => "video",
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso" => "archive",
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cs" | "java" | "go" | "html" | "css"
        | "json" | "toml" | "yaml" | "yml" | "xml" | "sh" | "ps1" | "bat" | "cmd" | "sql" | "lua" | "kt" | "php" => "code",
        "txt" | "md" | "pdf" | "doc" | "docx" | "odt" | "rtf" | "xls" | "xlsx" | "ods" | "csv" | "ppt" | "pptx" | "log" => "document",
        "exe" | "msi" | "dll" | "appx" | "lnk" | "com" | "scr" => "app",
        _ => "file",
    }
}
