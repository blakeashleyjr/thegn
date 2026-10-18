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
        for entry in entries.flatten() {
            if self.files.len() == 8 {
                break;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("zram") {
                continue;
            }
            let path = entry.path().join("mm_stat");
            let file = match ProcFile::open(path) {
                Ok(file) => Some(file),
                Err(_) => None,
            };
            self.files.push(ZramFile {
                name: name.into_owned(),
                file,
                mm: None,
            });
        }
        self.files.sort_by(|left, right| left.name.cmp(&right.name));
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
