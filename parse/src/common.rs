use std::fs;
use std::path::PathBuf;

pub fn collect_zips(args: &[String]) -> Vec<PathBuf> {
    if args.len() == 2 && args[0] == "--from-list" {
        let text = fs::read_to_string(&args[1]).unwrap_or_else(|e| {
            eprintln!("Failed to read {}: {e}", args[1]);
            std::process::exit(1);
        });
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(PathBuf::from)
            .collect()
    } else {
        args.iter().map(PathBuf::from).collect()
    }
}

pub struct Seen {
    ids: Vec<u32>,
}

impl Seen {
    pub fn from_ids(mut ids: Vec<u32>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self { ids }
    }

    pub fn contains(&self, id: u32) -> bool {
        self.ids.binary_search(&id).is_ok()
    }

    pub fn insert_all(&mut self, new: impl IntoIterator<Item = u32>) {
        let mut new: Vec<u32> = new.into_iter().collect();
        new.sort_unstable();
        new.dedup();
        if new.is_empty() {
            return;
        }
        let mut merged = Vec::with_capacity(self.ids.len() + new.len());
        let (mut left, mut right) = (0usize, 0usize);
        while left < self.ids.len() && right < new.len() {
            match self.ids[left].cmp(&new[right]) {
                std::cmp::Ordering::Less => {
                    merged.push(self.ids[left]);
                    left += 1;
                }
                std::cmp::Ordering::Greater => {
                    merged.push(new[right]);
                    right += 1;
                }
                std::cmp::Ordering::Equal => {
                    merged.push(self.ids[left]);
                    left += 1;
                    right += 1;
                }
            }
        }
        merged.extend_from_slice(&self.ids[left..]);
        merged.extend_from_slice(&new[right..]);
        self.ids = merged;
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }
}
