//! Kafka-compatible partitioned log and consumer group (#75).
//!
//! Offsets are integers, partitions are numbered, and a consumer group
//! stores its committed offsets next to the topic so a restarted worker
//! resumes from the same position. The store is a directory: tests and
//! local runs use it as the live topic without a broker.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::streaming::{ConsumerGroup, PartitionLog, Record};
use crate::streaming_fs::{DirLock, atomic_write_json, read_json, safe_component};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartitionFile {
    earliest: u64,
    records: Vec<StoredRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRecord {
    offset: u64,
    payload: Vec<u8>,
}

/// Durable partitioned log. `root` holds one topic.
#[derive(Debug, Clone)]
pub struct TopicLog {
    root: PathBuf,
}

impl TopicLog {
    /// Opens `root`, creating it when missing.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        let _lock = DirLock::acquire(&root)?;
        std::fs::create_dir_all(root.join("partitions"))
            .map_err(|error| format!("create topic log: {error}"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Appends `payload` on `partition` and returns its offset.
    pub fn append(&mut self, partition: u32, payload: Vec<u8>) -> Result<u64, String> {
        let _lock = DirLock::acquire(&self.root)?;
        let mut part = self.load(partition)?;
        let offset = part
            .records
            .last()
            .map(|record| record.offset + 1)
            .unwrap_or(part.earliest);
        part.records.push(StoredRecord { offset, payload });
        self.store(partition, &part)?;
        Ok(offset)
    }

    /// Drops retention below `earliest`. Records below it stay unread.
    pub fn set_earliest(&mut self, partition: u32, earliest: u64) -> Result<(), String> {
        let _lock = DirLock::acquire(&self.root)?;
        let mut part = self.load(partition)?;
        part.earliest = earliest;
        part.records.retain(|record| record.offset >= earliest);
        self.store(partition, &part)
    }

    fn path(&self, partition: u32) -> PathBuf {
        self.root
            .join("partitions")
            .join(format!("{partition}.json"))
    }

    fn load(&self, partition: u32) -> Result<PartitionFile, String> {
        Ok(read_json(&self.path(partition))?.unwrap_or(PartitionFile {
            earliest: 0,
            records: Vec::new(),
        }))
    }

    fn store(&self, partition: u32, part: &PartitionFile) -> Result<(), String> {
        atomic_write_json(&self.path(partition), part)
    }
}

impl PartitionLog for TopicLog {
    fn earliest(&mut self, partition: u32) -> Result<u64, String> {
        Ok(self.load(partition)?.earliest)
    }

    fn head(&mut self, partition: u32) -> Result<u64, String> {
        let part = self.load(partition)?;
        Ok(part
            .records
            .last()
            .map_or(part.earliest, |record| record.offset + 1))
    }

    fn read(&mut self, partition: u32, from: u64, max: usize) -> Result<Vec<Record>, String> {
        let part = self.load(partition)?;
        Ok(part
            .records
            .into_iter()
            .filter(|record| record.offset >= from)
            .take(max)
            .map(|record| Record {
                offset: record.offset,
                payload: record.payload,
            })
            .collect())
    }
}

/// Durable consumer-group offsets for a [`TopicLog`].
#[derive(Debug, Clone)]
pub struct TopicGroup {
    root: PathBuf,
    group: String,
}

impl TopicGroup {
    pub fn open(root: impl Into<PathBuf>, group: impl Into<String>) -> Result<Self, String> {
        let root = root.into();
        let group = safe_component(&group.into(), "consumer_group")?.to_string();
        let _lock = DirLock::acquire(&root)?;
        std::fs::create_dir_all(root.join("groups"))
            .map_err(|error| format!("create consumer group: {error}"))?;
        Ok(Self { root, group })
    }

    fn path(&self) -> PathBuf {
        self.root
            .join("groups")
            .join(format!("{}.json", self.group))
    }

    fn load(&self) -> Result<std::collections::BTreeMap<u32, u64>, String> {
        Ok(read_json(&self.path())?.unwrap_or_default())
    }

    fn store(&self, offsets: &std::collections::BTreeMap<u32, u64>) -> Result<(), String> {
        atomic_write_json(&self.path(), offsets)
    }
}

impl ConsumerGroup for TopicGroup {
    fn committed(&mut self, partition: u32) -> Result<Option<u64>, String> {
        let _lock = DirLock::acquire(&self.root)?;
        Ok(self.load()?.get(&partition).copied())
    }

    fn commit(&mut self, partition: u32, offset: u64) -> Result<(), String> {
        let _lock = DirLock::acquire(&self.root)?;
        let mut offsets = self.load()?;
        if offsets
            .get(&partition)
            .is_some_and(|current| offset < *current)
        {
            return Ok(());
        }
        offsets.insert(partition, offset);
        self.store(&offsets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reopened_log_keeps_offsets_and_the_group_commit() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = TopicLog::open(dir.path()).unwrap();
        assert_eq!(log.append(0, b"a".to_vec()).unwrap(), 0);
        assert_eq!(log.append(0, b"b".to_vec()).unwrap(), 1);
        assert_eq!(log.append(1, b"c".to_vec()).unwrap(), 0);
        let mut group = TopicGroup::open(dir.path(), "kako-orders").unwrap();
        group.commit(0, 1).unwrap();

        let mut log = TopicLog::open(dir.path()).unwrap();
        assert_eq!(log.earliest(0).unwrap(), 0);
        assert_eq!(log.head(0).unwrap(), 2);
        let records = log.read(0, 1, 8).unwrap();
        assert_eq!(
            records,
            vec![Record {
                offset: 1,
                payload: b"b".to_vec()
            }]
        );
        let mut group = TopicGroup::open(dir.path(), "kako-orders").unwrap();
        assert_eq!(group.committed(0).unwrap(), Some(1));
        group.commit(0, 4).unwrap();
        group.commit(0, 2).unwrap();
        assert_eq!(group.committed(0).unwrap(), Some(4));
        assert_eq!(group.committed(1).unwrap(), None);

        log.set_earliest(0, 1).unwrap();
        assert_eq!(log.earliest(0).unwrap(), 1);
        assert!(log.read(0, 0, 8).unwrap().iter().all(|r| r.offset >= 1));
    }

    #[test]
    fn a_group_name_cannot_escape_the_topic_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(TopicGroup::open(dir.path(), "../escape").is_err());
        assert!(TopicGroup::open(dir.path(), "a/b").is_err());
    }
}
