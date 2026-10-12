//! File categories: extension → category → default folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::filename::extension;

pub const GENERAL: &str = "General";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    pub extensions: Vec<String>,
    pub dir: PathBuf,
}

fn exts(list: &str) -> Vec<String> {
    list.split_whitespace().map(str::to_string).collect()
}

/// The default set, laid out under the user's Downloads folder.
pub fn defaults(downloads: &Path) -> Vec<Category> {
    let cat = |name: &str, list: &str| Category { name: name.into(), extensions: exts(list), dir: downloads.join(name) };
    vec![
        Category { name: GENERAL.into(), extensions: Vec::new(), dir: downloads.to_path_buf() },
        cat("Compressed", "zip rar 7z gz bz2 xz tar iso z arj ace lzh sit sitx sea tgz zst"),
        cat("Documents", "pdf doc docx xls xlsx ppt pptx pps txt rtf odt ods odp epub csv"),
        cat("Music", "mp3 wav wma flac aac m4a ogg opus aif ra mpa"),
        cat("Programs", "exe msi msu msix apk dmg bin deb rpm appimage"),
        cat("Video", "mp4 mkv avi mov wmv flv webm mpg mpeg m4v 3gp rm rmvb asf ogv ts"),
    ]
}

/// Category for a file name; `General` when no extension list claims it.
pub fn categorize<'a>(file_name: &str, categories: &'a [Category]) -> &'a str {
    let Some(ext) = extension(file_name) else { return GENERAL };
    categories
        .iter()
        .find(|c| c.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)))
        .map(|c| c.name.as_str())
        .unwrap_or(GENERAL)
}

pub fn dir_for<'a>(category: &str, categories: &'a [Category]) -> Option<&'a Path> {
    categories.iter().find(|c| c.name == category).map(|c| c.dir.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categorizes_by_extension() {
        let cats = defaults(Path::new("D:\\Downloads"));
        assert_eq!(categorize("ubuntu-24.04.ISO", &cats), "Compressed");
        assert_eq!(categorize("movie.final.mkv", &cats), "Video");
        assert_eq!(categorize("setup.exe", &cats), "Programs");
        assert_eq!(categorize("README", &cats), GENERAL);
        assert_eq!(categorize("data.unknownext", &cats), GENERAL);
        assert_eq!(dir_for("Video", &cats), Some(Path::new("D:\\Downloads\\Video")));
        assert_eq!(dir_for(GENERAL, &cats), Some(Path::new("D:\\Downloads")));
    }
}
