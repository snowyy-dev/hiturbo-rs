use crate::protocol::Frame;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureSettings {
    pub enabled: bool,
    pub sample: bool,
    pub every: u32,
    pub generation: u64,
}
impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            sample: false,
            every: 10,
            generation: 0,
        }
    }
}
#[derive(Clone, Default, Debug, Serialize)]
pub struct CaptureStats {
    pub received: u64,
    pub saved: u64,
    pub bytes: u64,
    pub folder: Option<PathBuf>,
}
pub struct FrameCapture {
    root: PathBuf,
    generation: Option<u64>,
    manifest: Option<BufWriter<File>>,
    pub stats: CaptureStats,
}
impl FrameCapture {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            generation: None,
            manifest: None,
            stats: CaptureStats::default(),
        }
    }
    pub fn accept(
        &mut self,
        f: &Frame,
        received: f64,
        settings: &CaptureSettings,
    ) -> Result<(), String> {
        if !settings.enabled {
            return Ok(());
        }
        if self.generation != Some(settings.generation) {
            let folder = self.root.join(format!("frames-{:04}", settings.generation));
            fs::create_dir(&folder).map_err(|e| e.to_string())?;
            self.manifest = Some(BufWriter::new(new_file(&folder.join("frames.jsonl"))?));
            self.stats = CaptureStats {
                folder: Some(folder),
                ..Default::default()
            };
            self.generation = Some(settings.generation);
        }
        let index = self.stats.received;
        self.stats.received += 1;
        let stride = if settings.sample {
            settings.every.max(1) as u64
        } else {
            1
        };
        if !index.is_multiple_of(stride) {
            return Ok(());
        }
        let name = format!("frame-{:08}.jpg", index + 1);
        let path = self.stats.folder.as_ref().unwrap().join(&name);
        let mut out = new_file(&path)?;
        out.write_all(&f.jpeg).map_err(|e| e.to_string())?;
        let entry = serde_json::json!({"file":name,"capture_frame":index+1,"received_unix_seconds":received,
            "sequence":f.sequence,"timestamp_raw":f.timestamp_raw,"bytes":f.jpeg.len(),
            "orientation":"original camera bytes","advertised_width":f.advertised_width,"advertised_height":f.advertised_height});
        writeln!(self.manifest.as_mut().unwrap(), "{entry}").map_err(|e| e.to_string())?;
        self.stats.saved += 1;
        self.stats.bytes += f.jpeg.len() as u64;
        Ok(())
    }
    pub fn flush(&mut self) -> Result<(), String> {
        if let Some(m) = &mut self.manifest {
            m.flush().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
pub fn new_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}
pub fn metadata(data: &[u8]) -> serde_json::Value {
    let dimensions = image::load_from_memory(data)
        .ok()
        .map(|im| [im.width(), im.height()]);
    let mut tags = serde_json::Map::new();
    match exif::Reader::new().read_from_container(&mut Cursor::new(data)) {
        Ok(exif) => {
            for f in exif.fields() {
                tags.insert(
                    format!("{} / {}", f.ifd_num, f.tag),
                    serde_json::Value::String(f.display_value().with_unit(&exif).to_string()),
                );
            }
        }
        Err(exif::Error::NotFound(_)) => {}
        Err(e) => {
            tags.insert(
                "Parser note".into(),
                serde_json::Value::String(e.to_string()),
            );
        }
    }
    serde_json::json!({"dimensions":dimensions,"exif":tags})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_selection_is_before_preview_and_preserves_original_bytes() {
        let root = std::env::temp_dir().join(format!(
            "hiturbo-capture-test-{}",
            crate::session::unique_id()
        ));
        fs::create_dir(&root).unwrap();
        let mut c = FrameCapture::new(root.clone());
        let f = Frame {
            kind: 0xa1,
            format: 0,
            sequence: 0,
            advertised_width: 640,
            advertised_height: 360,
            timestamp_raw: 9,
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
        };
        let s = CaptureSettings {
            enabled: true,
            sample: true,
            every: 3,
            generation: 1,
        };
        for i in 0..10 {
            c.accept(&f, i as f64, &s).unwrap();
        }
        c.flush().unwrap();
        assert_eq!((c.stats.received, c.stats.saved), (10, 4));
        for index in [1, 4, 7, 10] {
            assert_eq!(
                fs::read(root.join(format!("frames-0001/frame-{index:08}.jpg"))).unwrap(),
                f.jpeg
            );
        }
        assert!(!root.join("frames-0001/frame-00000002.jpg").exists());
        drop(c);
        fs::remove_dir_all(root).unwrap();
    }
}
