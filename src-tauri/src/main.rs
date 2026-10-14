#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod scan;
mod store;

use rayon::prelude::*;
use scan::*;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

/// An incremental rescan trusts directory mtimes; force a full one at least this often.
const FULL_RESCAN_EVERY_SECS: u64 = 7 * 86400;

struct AppState {
    drives: Mutex<HashMap<String, Arc<DriveData>>>,
    scanning: AtomicBool,
    save_lock: Mutex<()>,
    db_path: PathBuf,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FolderView {
    name: String,
    size: u64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DriveView {
    letter: String,
    total: u64,
    free: u64,
    scanned_at: u64,
    scanned_bytes: u64,
    has_data: bool,
    folders: Vec<FolderView>,
}

#[derive(Serialize)]
struct FileView {
    path: String,
    size: u64,
}

#[derive(Serialize, Clone)]
struct ProgressView {
    drive: String,
    files: u64,
    dirs: u64,
    bytes: u64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn view(info: &DriveInfo, data: Option<&DriveData>) -> DriveView {
    let mut v = DriveView {
        letter: info.letter.clone(),
        total: info.total,
        free: info.free,
        scanned_at: 0,
        scanned_bytes: 0,
        has_data: false,
        folders: Vec::new(),
    };
    if let Some(d) = data {
        let mut kids: Vec<&Node> = d.root.children.iter().collect();
        kids.sort_by_key(|n| Reverse(n.size));
        v.scanned_at = d.scanned_at;
        v.scanned_bytes = d.root.size;
        v.has_data = true;
        v.folders = kids.into_iter().take(TOP_N).map(|n| FolderView { name: n.name.clone(), size: n.size }).collect();
    }
    v
}

#[tauri::command]
fn get_state(state: State<AppState>) -> Vec<DriveView> {
    let map = state.drives.lock().unwrap();
    list_drives().iter().map(|i| view(i, map.get(&i.letter).map(|a| &**a))).collect()
}

#[tauri::command]
fn start_scan(app: tauri::AppHandle, state: State<AppState>, full: bool) {
    if state.scanning.swap(true, SeqCst) {
        return;
    }
    std::thread::spawn(move || run_scan(app, full));
}

fn run_scan(app: tauri::AppHandle, full_requested: bool) {
    let state = app.state::<AppState>();
    let infos = list_drives();
    let progress: Vec<Arc<Progress>> = infos.iter().map(|_| Arc::new(Progress::default())).collect();
    let done = Arc::new(AtomicBool::new(false));

    let emitter = {
        let (app, done) = (app.clone(), done.clone());
        let named: Vec<(String, Arc<Progress>)> =
            infos.iter().map(|i| i.letter.clone()).zip(progress.iter().cloned()).collect();
        std::thread::spawn(move || loop {
            let finished = done.load(SeqCst);
            let snap: Vec<ProgressView> = named
                .iter()
                .map(|(l, p)| ProgressView {
                    drive: l.clone(),
                    files: p.files.load(SeqCst),
                    dirs: p.dirs.load(SeqCst),
                    bytes: p.bytes.load(SeqCst),
                })
                .collect();
            let _ = app.emit("scan-progress", snap);
            if finished {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        })
    };

    // Directory walking is I/O-latency bound, so oversubscribe the cores.
    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads((cpus * 4).clamp(16, 64))
        .stack_size(16 << 20)
        .build()
        .expect("thread pool");

    pool.install(|| {
        infos.par_iter().zip(progress.par_iter()).for_each(|(info, prog)| {
            let prev = state.drives.lock().unwrap().get(&info.letter).cloned();
            let now = now_secs();
            let full = full_requested
                || prev.as_ref().map_or(true, |p| now.saturating_sub(p.full_at) > FULL_RESCAN_EVERY_SECS);
            let root_path = PathBuf::from(format!("{}\\", info.letter));
            let mtime = std::fs::metadata(&root_path).map_or(0, |m| mtime_of(&m));
            let root = scan_dir(&root_path, info.letter.clone(), mtime, prev.as_ref().map(|d| &d.root), full, prog);
            let data = Arc::new(DriveData {
                letter: info.letter.clone(),
                scanned_at: now,
                full_at: if full { now } else { prev.map_or(now, |p| p.full_at) },
                root,
            });
            let snapshot: Vec<Arc<DriveData>> = {
                let mut g = state.drives.lock().unwrap();
                g.insert(info.letter.clone(), data.clone());
                g.values().cloned().collect()
            };
            {
                let _g = state.save_lock.lock().unwrap();
                let _ = store::save(&state.db_path, &snapshot);
            }
            let _ = app.emit("drive-done", view(info, Some(&data)));
        });
    });

    done.store(true, SeqCst);
    let _ = emitter.join();
    state.scanning.store(false, SeqCst);
    let _ = app.emit("scan-finished", ());
}

#[tauri::command(async)]
fn top_files(state: State<AppState>, drive: String, folder: String, limit: usize) -> Vec<FileView> {
    let limit = limit.clamp(1, STORE_TOP);
    let Some(data) = state.drives.lock().unwrap().get(&drive).cloned() else { return Vec::new() };
    let Some(node) = data.root.children.iter().find(|c| c.name == folder) else { return Vec::new() };

    fn walk(n: &Node, prefix: &mut String, heap: &mut TopHeap, limit: usize) {
        for (size, name) in &n.top {
            push_top(heap, limit, *size, || format!("{prefix}{name}"));
        }
        for c in &n.children {
            let len = prefix.len();
            prefix.push_str(&c.name);
            prefix.push('\\');
            walk(c, prefix, heap, limit);
            prefix.truncate(len);
        }
    }
    let mut heap = TopHeap::new();
    walk(node, &mut String::new(), &mut heap, limit);
    heap.into_sorted_vec().into_iter().map(|r| FileView { path: r.0 .1, size: r.0 .0 }).collect()
}

/// Build an absolute path from a drive letter and a drive-relative path, refusing anything
/// that could escape the drive or isn't a plain sequence of names.
fn resolve(drive: &str, rel: &str) -> Result<PathBuf, String> {
    let ok_drive = drive.len() == 2 && drive.as_bytes()[0].is_ascii_alphabetic() && drive.ends_with(':');
    let ok_rel = !rel.is_empty()
        && rel.split('\\').all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains([':', '/', '*', '?', '"', '<', '>', '|']));
    if !ok_drive || !ok_rel {
        return Err("invalid path".into());
    }
    Ok(PathBuf::from(format!("{drive}\\{rel}")))
}

#[tauri::command]
fn reveal(drive: String, rel: String, is_file: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let p = resolve(&drive, &rel)?;
    let mut cmd = std::process::Command::new("explorer.exe");
    if is_file {
        cmd.raw_arg(format!("/select,\"{}\"", p.display()));
    } else {
        cmd.raw_arg(format!("\"{}\"", p.display()));
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Moves to the Recycle Bin (undoable). Items too big for the bin are refused rather than erased.
#[tauri::command(async)]
fn delete_path(state: State<AppState>, drive: String, rel: String) -> Result<DriveView, String> {
    use windows_sys::Win32::UI::Shell::*;
    if state.scanning.load(SeqCst) {
        return Err("a scan is running; try again when it finishes".into());
    }
    let p = resolve(&drive, &rel)?;
    let is_dir = p.is_dir();
    if !p.exists() {
        return Err("already gone".into());
    }
    let mut from: Vec<u16> = p.to_string_lossy().encode_utf16().collect();
    from.extend([0, 0]);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16;
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 || op.fAnyOperationsAborted != 0 {
        return Err(format!("could not move to Recycle Bin (code {rc})"));
    }

    // Update the stored tree in place instead of rescanning.
    let comps: Vec<&str> = rel.split('\\').collect();
    let (data, snapshot) = {
        let mut g = state.drives.lock().unwrap();
        let arc = g.get_mut(&drive).ok_or("drive not scanned")?;
        let d = Arc::make_mut(arc); // no copy unless something else still holds this snapshot
        apply_delete(&mut d.root, &PathBuf::from(format!("{drive}\\")), &comps, is_dir);
        let data = arc.clone();
        (data, g.values().cloned().collect::<Vec<_>>())
    };
    {
        let _g = state.save_lock.lock().unwrap();
        let _ = store::save(&state.db_path, &snapshot);
    }
    let info = list_drives().into_iter().find(|i| i.letter == drive).ok_or("drive not found")?;
    Ok(view(&info, Some(&data)))
}

fn main() {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let db_path = exe_dir.join("spacehog.db");
    let webview_dir = exe_dir.join("spacehog.webview");

    let drives: HashMap<String, Arc<DriveData>> =
        store::load(&db_path).into_iter().map(|d| (d.letter.clone(), Arc::new(d))).collect();

    let app = tauri::Builder::default()
        .manage(AppState {
            drives: Mutex::new(drives),
            scanning: AtomicBool::new(false),
            save_lock: Mutex::new(()),
            db_path,
        })
        .invoke_handler(tauri::generate_handler![get_state, start_scan, top_files, reveal, delete_path])
        .setup(move |app| {
            // Keep WebView2's cache next to the exe instead of in %LOCALAPPDATA%.
            let _ = std::fs::remove_dir_all(&webview_dir);
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("SpaceHog")
                .inner_size(780.0, 900.0)
                .min_inner_size(520.0, 400.0)
                .data_directory(webview_dir)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build SpaceHog");

    let cleanup = exe_dir.join("spacehog.webview");
    app.run(move |_, ev| {
        if let tauri::RunEvent::Exit = ev {
            let c = cleanup.clone();
            // WebView2 processes may still hold files for a moment; the next launch retries anyway.
            let _ = std::fs::remove_dir_all(c);
        }
    });
}
