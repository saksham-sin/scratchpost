use crate::protocol::{
    BarrierState, Config, IntentRecord, LedgerSnapshot, Notice, PathClaim, SequenceClaim,
    SessionRecord,
};
use memmap2::{Mmap, MmapMut};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: u64 = 0x53504c4544475231; // "SPLEDGR1"
const LAYOUT_VERSION: u64 = 1;
const HEADER_SIZE: usize = 4096;
const SLOT_SIZE: usize = 512 * 1024;
const FILE_SIZE: usize = HEADER_SIZE + SLOT_SIZE * 2;
const MAGIC_OFF: usize = 0;
const VERSION_OFF: usize = 8;
const GENERATION_OFF: usize = 16;
const ACTIVE_OFF: usize = 24;
const LEN_OFF: usize = 32;
const SEQ_OFF: usize = 48;

fn atomic_at(mmap: &MmapMut, offset: usize) -> &AtomicU64 {
    unsafe { &*(mmap.as_ptr().add(offset) as *const AtomicU64) }
}

fn atomic_at_read(mmap: &Mmap, offset: usize) -> &AtomicU64 {
    unsafe { &*(mmap.as_ptr().add(offset) as *const AtomicU64) }
}

fn slot_offset(slot: usize) -> usize {
    HEADER_SIZE + slot * SLOT_SIZE
}

fn seq_offset(slot: usize) -> usize {
    SEQ_OFF + slot * 8
}

fn len_offset(slot: usize) -> usize {
    LEN_OFF + slot * 8
}

pub struct LedgerStore {
    mmap: MmapMut,
    generation: u64,
}

impl LedgerStore {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if file.metadata()?.len() != FILE_SIZE as u64 {
            file.set_len(FILE_SIZE as u64)?;
        }
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let magic = atomic_at(&mmap, MAGIC_OFF).load(Ordering::Acquire);
        if magic != MAGIC {
            atomic_at(&mmap, MAGIC_OFF).store(MAGIC, Ordering::Release);
            atomic_at(&mmap, VERSION_OFF).store(LAYOUT_VERSION, Ordering::Release);
            atomic_at(&mmap, ACTIVE_OFF).store(0, Ordering::Release);
            atomic_at(&mmap, GENERATION_OFF).store(0, Ordering::Release);
            atomic_at(&mmap, len_offset(0)).store(0, Ordering::Release);
            atomic_at(&mmap, len_offset(1)).store(0, Ordering::Release);
            atomic_at(&mmap, seq_offset(0)).store(0, Ordering::Release);
            atomic_at(&mmap, seq_offset(1)).store(0, Ordering::Release);
        }
        let generation = atomic_at(&mmap, GENERATION_OFF).load(Ordering::Acquire);
        Ok(Self { mmap, generation })
    }

    pub fn publish(&mut self, snapshot: &LedgerSnapshot) -> std::io::Result<u64> {
        let bytes = serde_json::to_vec(snapshot).map_err(std::io::Error::other)?;
        if bytes.len() > SLOT_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("ledger snapshot too large: {} bytes", bytes.len()),
            ));
        }
        let active = atomic_at(&self.mmap, ACTIVE_OFF).load(Ordering::Acquire) as usize;
        let target = 1 - active;
        atomic_at(&self.mmap, seq_offset(target)).fetch_add(1, Ordering::AcqRel);
        let offset = slot_offset(target);
        self.mmap[offset..offset + bytes.len()].copy_from_slice(&bytes);
        atomic_at(&self.mmap, len_offset(target)).store(bytes.len() as u64, Ordering::Release);
        atomic_at(&self.mmap, seq_offset(target)).fetch_add(1, Ordering::AcqRel);
        atomic_at(&self.mmap, ACTIVE_OFF).store(target as u64, Ordering::Release);
        self.generation = atomic_at(&self.mmap, GENERATION_OFF).fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.mmap.flush_async();
        Ok(self.generation)
    }
}

pub fn read_snapshot_path(path: &Path) -> Option<(LedgerSnapshot, u64)> {
    let file = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    if mmap.len() < FILE_SIZE {
        return None;
    }
    if atomic_at_read(&mmap, MAGIC_OFF).load(Ordering::Acquire) != MAGIC
        || atomic_at_read(&mmap, VERSION_OFF).load(Ordering::Acquire) != LAYOUT_VERSION
    {
        return None;
    }
    for _ in 0..128 {
        let active = atomic_at_read(&mmap, ACTIVE_OFF).load(Ordering::Acquire) as usize;
        if active > 1 {
            return None;
        }
        let seq = atomic_at_read(&mmap, seq_offset(active)).load(Ordering::Acquire);
        if seq % 2 == 1 {
            std::thread::yield_now();
            continue;
        }
        let len = atomic_at_read(&mmap, len_offset(active)).load(Ordering::Acquire) as usize;
        if len == 0 || len > SLOT_SIZE {
            return None;
        }
        let offset = slot_offset(active);
        let bytes = &mmap[offset..offset + len];
        let parsed = serde_json::from_slice::<LedgerSnapshot>(bytes);
        let seq_after = atomic_at_read(&mmap, seq_offset(active)).load(Ordering::Acquire);
        if seq != seq_after {
            continue;
        }
        if let Ok(snapshot) = parsed {
            let generation = atomic_at_read(&mmap, GENERATION_OFF).load(Ordering::Acquire);
            return Some((snapshot, generation));
        }
    }
    None
}

pub fn read_snapshot(master: &Path) -> Option<(LedgerSnapshot, u64)> {
    read_snapshot_path(&crate::workspace::mmap_path(master))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WalEvent {
    Config { config: Config },
    Session { session: SessionRecord },
    Claim { claim: SequenceClaim },
    Intent { intent: IntentRecord },
    Notice { notice: Notice },
    Barrier { barrier: BarrierState },
    PathClaim { claim: PathClaim },
}

pub struct Wal {
    file: File,
}

impl Wal {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn append(&mut self, event: &WalEvent) -> std::io::Result<()> {
        let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    pub fn append_durable(&mut self, event: &WalEvent) -> std::io::Result<()> {
        self.append(event)?;
        self.file.sync_data()?;
        Ok(())
    }
}

pub fn replay(path: &Path) -> Vec<WalEvent> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let mut contents = String::new();
    if file.read_to_string(&mut contents).is_err() {
        return Vec::new();
    }
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<WalEvent>(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Mode, SessionStatus};
    use std::sync::Arc;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "scratchpost-test-{}-{}-{}",
            tag,
            std::process::id(),
            crate::protocol::now_ms()
        ))
    }

    fn sample_snapshot(n: u64) -> LedgerSnapshot {
        LedgerSnapshot {
            daemon_pid: std::process::id(),
            started_ms: n,
            master_root: "/tmp/master".to_string(),
            config: Config {
                mode: if n % 2 == 0 {
                    Mode::Fcfs
                } else {
                    Mode::BarrierStacked
                },
                ..Config::default()
            },
            sessions: vec![SessionRecord {
                agent: "agent_1".to_string(),
                session_id: "s1".to_string(),
                slot: 1,
                port_base: 31_000,
                workspace: "/tmp/ws".to_string(),
                created_ms: n,
                last_heartbeat_ms: n,
                status: SessionStatus::Active,
                repos: Vec::new(),
            }],
            claims: Vec::new(),
            intents: Vec::new(),
            notices: Vec::new(),
            barrier: BarrierState::default(),
            barriers: Vec::new(),
            components: Vec::new(),
            path_claims: Vec::new(),
        }
    }

    #[test]
    fn seqlock_concurrent_reads() {
        let path = temp_path("seqlock");
        let store = Arc::new(std::sync::Mutex::new(LedgerStore::create(&path).unwrap()));
        let writer = std::thread::spawn(move || {
            let mut store = store.lock().unwrap();
            for n in 0..64 {
                store.publish(&sample_snapshot(n)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0;
            let mut last_generation = 0;
            for _ in 0..2000 {
                if let Some((snapshot, generation)) = read_snapshot_path(&reader_path) {
                    if generation >= last_generation {
                        last_generation = generation;
                    }
                    assert!(snapshot.daemon_pid == std::process::id());
                    reads += 1;
                    if reads > 20 {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(reads > 0);
            assert!(last_generation > 0);
        });
        writer.join().unwrap();
        reader.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wal_recovery_replays_events() {
        let path = temp_path("wal");
        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&WalEvent::Config {
                config: Config::default(),
            })
            .unwrap();
            wal.append(&WalEvent::Barrier {
                barrier: BarrierState::default(),
            })
            .unwrap();
        }
        let events = replay(&path);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], WalEvent::Config { .. }));
        assert!(matches!(events[1], WalEvent::Barrier { .. }));
        let _ = std::fs::remove_file(&path);
    }
}
