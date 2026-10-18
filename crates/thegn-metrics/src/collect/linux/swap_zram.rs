//! Held zram `mm_stat` readers used by the memory collector.

use std::{fs, path::Path};

use super::procfile::ProcFile;
use crate::collect::{model::meminfo::ZramDev, parse::zram};

#[derive(Debug, Default)]
struct ZramFile {
    name: String,
    file: Option<ProcFile>,
    mm: Option<zram::ZramMmStat>,
}

#[derive(Debug, Default)]
pub struct ZramFiles {
    files: Vec<ZramFile>,
}

impl ZramFiles {
    pub fn discover(&mut self, block_root: &Path) {
        self.clear();
        let Ok(entries) = fs::read_dir(block_root) else {
            return;
        };
        let mut names: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("zram")
                    .then_some((name.into_owned(), entry.path().join("mm_stat")))
            })
            .collect();
        names.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, path) in names.into_iter().take(8) {
            let file = ProcFile::open(path).ok();
            self.files.push(ZramFile {
                name,
                file,
                mm: None,
            });
        }
    }

    pub fn read_all(&mut self) {
        for device in &mut self.files {
            let (mm, reopen) = match device.file.as_mut() {
                Some(file) => match file.read_all() {
                    Ok(bytes) => (zram::parse_mm_stat(bytes), false),
                    Err(_) => (None, file.needs_reopen()),
                },
                None => (None, false),
            };
            device.mm = mm;
            if reopen {
                device.file = None;
            }
        }
    }

    pub fn publish(&self, out: &mut Vec<ZramDev>) {
        for (index, device) in self.files.iter().enumerate() {
            if let Some(target) = out.get_mut(index) {
                if target.name != device.name {
                    target.name.clear();
                    target.name.push_str(&device.name);
                }
                target.mm = device.mm;
            } else {
                out.push(ZramDev {
                    name: device.name.clone(),
                    mm: device.mm,
                });
            }
        }
        out.truncate(self.files.len());
    }

    pub fn clear(&mut self) {
        self.files.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::ZramFiles;
    use std::fs;

    #[test]
    fn discovery_sorts_names_before_applying_cap() {
        let dir = tempfile::tempdir().unwrap();
        for index in (0..10).rev() {
            let device = dir.path().join(format!("zram{index:02}"));
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("mm_stat"), b"1 1 1 0 1 0 0\n").unwrap();
        }
        let mut files = ZramFiles::default();
        files.discover(dir.path());
        files.read_all();
        let mut published = Vec::new();
        files.publish(&mut published);
        assert_eq!(published.len(), 8);
        assert_eq!(
            published
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>(),
            [
                "zram00", "zram01", "zram02", "zram03", "zram04", "zram05", "zram06", "zram07"
            ]
        );
    }
}
