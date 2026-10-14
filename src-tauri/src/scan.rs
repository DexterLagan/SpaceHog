use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::UNIX_EPOCH;

pub const TOP_N: usize = 10;

/// One directory. `top` holds the largest files directly inside it (descending),
/// `size` is the aggregate of everything below.
#[derive(Serialize, Deserialize, Default)]
pub struct Node {
    pub name: String,
    pub mtime: u64,
    pub own_size: u64,
    pub own_files: u32,
    pub top: Vec<(u64, String)>,
    pub size: u64,
    pub children: Vec<Node>,
}

#[derive(Serialize, Deserialize)]
pub struct DriveData {
    pub letter: String,
    pub scanned_at: u64,
    pub full_at: u64,
    pub root: Node,
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
}

pub type TopHeap = BinaryHeap<Reverse<(u64, String)>>;

/// Push (size, name) if it belongs in the top N. The name closure only runs when it does.
pub fn push_top(heap: &mut TopHeap, size: u64, name: impl FnOnce() -> String) {
    if heap.len() < TOP_N || size > heap.peek().map_or(0, |r| r.0 .0) {
        heap.push(Reverse((size, name())));
        if heap.len() > TOP_N {
            heap.pop();
        }
    }
}

pub fn mtime_of(md: &Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Bytes a file actually occupies locally. Cloud placeholders (OneDrive "online-only") take none;
/// sparse/compressed files are asked for their allocated size. Plain files cost no extra syscall.
fn disk_size(entry: &fs::DirEntry, md: &Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x40000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    const SPARSE_OR_COMPRESSED: u32 = 0x200 | 0x800;
    let attrs = md.file_attributes();
    if attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        return 0;
    }
    if attrs & SPARSE_OR_COMPRESSED != 0 {
        let wide: Vec<u16> = entry.path().as_os_str().encode_wide_nul();
        let mut high = 0u32;
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        if low != u32::MAX || std::io::Error::last_os_error().raw_os_error() == Some(0) {
            return ((high as u64) << 32) | low as u64;
        }
    }
    md.len()
}

trait WideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}
impl WideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

/// Scan a directory in parallel. If `prev` is given and the directory mtime is unchanged
/// (and `full` is false), its direct files are reused and only the subdirectories are visited.
pub fn scan_dir(
    path: &Path,
    name: String,
    mtime: u64,
    prev: Option<&Node>,
    full: bool,
    p: &Progress,
) -> Node {
    let mut node = Node { name, mtime, ..Default::default() };
    p.dirs.fetch_add(1, Relaxed);

    match prev.filter(|pv| !full && pv.mtime == mtime) {
        Some(pv) => {
            node.own_size = pv.own_size;
            node.own_files = pv.own_files;
            node.top = pv.top.clone();
            p.files.fetch_add(pv.own_files as u64, Relaxed);
            p.bytes.fetch_add(pv.own_size, Relaxed);
            node.children = pv
                .children
                .par_iter()
                .filter_map(|c| {
                    let cp = path.join(&c.name);
                    let md = fs::symlink_metadata(&cp).ok()?;
                    if !md.is_dir() || md.file_type().is_symlink() {
                        return None;
                    }
                    Some(scan_dir(&cp, c.name.clone(), mtime_of(&md), Some(c), full, p))
                })
                .collect();
        }
        None => {
            let Ok(rd) = fs::read_dir(path) else { return node };
            let mut heap = TopHeap::new();
            let mut subdirs: Vec<(PathBuf, String, u64)> = Vec::new();
            for entry in rd.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    continue; // symlinks and junctions: don't follow, avoids loops and double counting
                }
                let Ok(md) = entry.metadata() else { continue };
                if ft.is_dir() {
                    subdirs.push((entry.path(), entry.file_name().to_string_lossy().into_owned(), mtime_of(&md)));
                } else {
                    let len = disk_size(&entry, &md);
                    node.own_size += len;
                    node.own_files += 1;
                    push_top(&mut heap, len, || entry.file_name().to_string_lossy().into_owned());
                }
            }
            p.files.fetch_add(node.own_files as u64, Relaxed);
            p.bytes.fetch_add(node.own_size, Relaxed);
            node.top = heap.into_sorted_vec().into_iter().map(|r| r.0).collect();

            let prev_map: Option<HashMap<&str, &Node>> =
                prev.map(|pv| pv.children.iter().map(|c| (c.name.as_str(), c)).collect());
            node.children = subdirs
                .into_par_iter()
                .map(|(cp, cn, cm)| {
                    let pc = prev_map.as_ref().and_then(|m| m.get(cn.as_str()).copied());
                    scan_dir(&cp, cn, cm, pc, full, p)
                })
                .collect();
        }
    }

    node.size = node.own_size + node.children.iter().map(|c| c.size).sum::<u64>();
    node
}

pub struct DriveInfo {
    pub letter: String,
    pub total: u64,
    pub free: u64,
}

/// Fixed (non-removable, non-network) drives.
pub fn list_drives() -> Vec<DriveInfo> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mut out = Vec::new();
    unsafe {
        let mask = GetLogicalDrives();
        for i in 0..26u8 {
            if mask >> i & 1 == 0 {
                continue;
            }
            let letter = (b'A' + i) as char;
            let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain(Some(0)).collect();
            if GetDriveTypeW(root.as_ptr()) != DRIVE_FIXED {
                continue;
            }
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            if GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) != 0 {
                out.push(DriveInfo { letter: format!("{letter}:"), total, free });
            }
        }
    }
    out
}
