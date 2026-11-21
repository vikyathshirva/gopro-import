//! Tiny persistent settings (currently just the import folder).
use std::{fs, path::{Path, PathBuf}};

fn file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gopro-import").join("config"))
}

pub fn default_dest() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join("Movies").join("GoPro")
}

pub fn expand(p: &str) -> PathBuf {
    let p = p.trim();
    if p == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(p));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

/// Saved import folder, if any.
pub fn load_dest() -> Option<PathBuf> {
    let s = fs::read_to_string(file()?).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("dest=").map(|v| expand(v)))
        .filter(|p| !p.as_os_str().is_empty())
}

pub fn save_dest(p: &Path) -> std::io::Result<()> {
    let f = file().ok_or_else(|| std::io::Error::other("no config dir"))?;
    if let Some(d) = f.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(f, format!("dest={}\n", p.display()))
}
