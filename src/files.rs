use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Write `bytes` unless the file already holds exactly them. Unchanged outputs keep their mtime, so the
/// rsync mirror skips them. Returns whether the file was written
pub fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.is_file()
            && meta.len() == bytes.len() as u64
            && fs::read(path).is_ok_and(|old| old == bytes)
        {
            return Ok(false);
        }
    } else if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

/// Make `dest` hold the same bytes as `src`: a hard link where the filesystem allows, else a copy
/// A `dest` that is a separate copy (written by older builds) is replaced by a link, so its disk space
/// is reclaimed and later runs need only compare inodes. Without hard links, an identical copy is left
/// alone so its mtime survives
pub fn link_or_copy(src: &Path, dest: &Path) -> Result<(), String> {
    if same_file(src, dest) {
        return Ok(());
    }
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: no file name", dest.display()))?;
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    // Link under a temporary name and rename it over `dest`, so `dest` never goes missing
    let tmp = parent.join(format!(".{name}.link"));
    let _ = fs::remove_file(&tmp);
    if fs::hard_link(src, &tmp).is_ok() {
        return fs::rename(&tmp, dest).map_err(|e| format!("{}: {e}", dest.display()));
    }
    let same_len = match (fs::metadata(src), fs::metadata(dest)) {
        (Ok(a), Ok(b)) => a.len() == b.len(),
        _ => false,
    };
    if same_len
        && fs::read(src)
            .ok()
            .is_some_and(|a| fs::read(dest).is_ok_and(|b| a == b))
    {
        return Ok(());
    }
    fs::copy(src, dest)
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", dest.display()))
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_: &Path, _: &Path) -> bool {
    false
}

/// Delete every file under `dir` that is not in `keep`, then any directories left empty
pub fn prune(dir: &Path, keep: &HashSet<PathBuf>) -> Result<usize, String> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0;
    let mut dirs = vec![dir.to_path_buf()];
    let mut i = 0;
    while i < dirs.len() {
        for ent in fs::read_dir(&dirs[i]).map_err(|e| e.to_string())? {
            let path = ent.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if !keep.contains(&path) {
                fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                removed += 1;
            }
        }
        i += 1;
    }
    // Children were pushed after their parents, so walking backwards empties leaves first
    for d in dirs.iter().skip(1).rev() {
        let _ = fs::remove_dir(d);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_mtime_of_unchanged_files_and_prunes_the_rest() {
        let dir = std::env::temp_dir().join(format!("phi-files-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = dir.join("x/a.bin");
        assert!(write_if_changed(&a, b"one").unwrap());
        let before = fs::metadata(&a).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!write_if_changed(&a, b"one").unwrap());
        assert_eq!(fs::metadata(&a).unwrap().modified().unwrap(), before);
        let b = dir.join("y/b.bin");
        // A separate copy with the same bytes (as older builds wrote) becomes a link
        write_if_changed(&b, b"one").unwrap();
        assert!(!same_file(&a, &b));
        link_or_copy(&a, &b).unwrap();
        assert!(same_file(&a, &b));
        assert!(!dir.join("y/.b.bin.link").exists());
        assert!(write_if_changed(&a, b"two!").unwrap());
        link_or_copy(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"two!");
        write_if_changed(&dir.join("x/stale/c.bin"), b"old").unwrap();
        let keep: HashSet<PathBuf> = [a.clone(), b.clone()].into_iter().collect();
        assert_eq!(prune(&dir, &keep).unwrap(), 1);
        assert!(!dir.join("x/stale").exists());
        assert!(a.is_file() && b.is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}
