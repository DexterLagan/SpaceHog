use crate::scan::DriveData;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

const VERSION: u32 = 3;

#[derive(Serialize)]
struct DbOut<'a> {
    version: u32,
    drives: Vec<&'a DriveData>,
}

#[derive(Deserialize)]
struct DbIn {
    version: u32,
    drives: Vec<DriveData>,
}

pub fn load(path: &Path) -> Vec<DriveData> {
    let Ok(f) = File::open(path) else { return Vec::new() };
    let dec = lz4_flex::frame::FrameDecoder::new(BufReader::new(f));
    match bincode::deserialize_from::<_, DbIn>(dec) {
        Ok(db) if db.version == VERSION => db.drives,
        _ => Vec::new(), // missing, corrupt or from another version: just rescan
    }
}

pub fn save(path: &Path, drives: &[Arc<DriveData>]) -> std::io::Result<()> {
    let tmp = path.with_extension("db.tmp");
    let out = DbOut { version: VERSION, drives: drives.iter().map(|d| &**d).collect() };
    let mut enc = lz4_flex::frame::FrameEncoder::new(BufWriter::new(File::create(&tmp)?));
    bincode::serialize_into(&mut enc, &out).map_err(std::io::Error::other)?;
    let mut w = enc.finish().map_err(std::io::Error::other)?;
    w.flush()?;
    drop(w);
    fs::rename(&tmp, path)
}
