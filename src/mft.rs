// Fast scan of a whole NTFS volume by reading its Master File Table directly,
// the way WizTree does. Needs administrator rights to open the raw volume.
//
// Worker threads read the MFT in chunks and parse the file records; a single
// builder thread inserts every record into the tree as soon as its parent
// folder is known, so the treemap keeps growing live while the MFT is read.

use crate::scan::{BATCH_WINDOW, ScanShared};
use crate::types::{NO_NODE, NodeID, Tree};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const SW_SHOWNORMAL: i32 = 1;

const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;
const RECORD_IN_USE: u16 = 0x01;
const RECORD_IS_DIRECTORY: u16 = 0x02;
const NAMESPACE_DOS: u8 = 2;
const REF_MASK: u64 = 0xFFFF_FFFF_FFFF;
const ROOT_RECORD: u32 = 5;

const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const ALIGNMENT: usize = 4096;
// measured on NVMe drives (one BitLocker encrypted): throughput stops growing
// at 2-4 readers, more only queue parsed chunks up in memory
const MAX_READERS: usize = 4;
// runs of unused records shorter than this are read through rather than
// splitting the read
const MERGE_GAP_BYTES: u64 = 256 * 1024;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        device: *mut c_void,
        code: u32,
        in_buf: *const c_void,
        in_size: u32,
        out_buf: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_cmd: i32,
    ) -> isize;
}

struct Tuning {
    readers: usize,
    chunk_bytes: usize,
    buffered: bool,
    use_bitmap: bool,
}

#[cfg(not(test))]
fn tuning() -> Tuning {
    Tuning {
        readers: default_readers(),
        chunk_bytes: CHUNK_BYTES,
        buffered: false,
        use_bitmap: true,
    }
}

// the benchmark test can try other settings through the environment
#[cfg(test)]
fn tuning() -> Tuning {
    let var = |name| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    };
    Tuning {
        readers: var("SPACEMAN_MFT_READERS")
            .unwrap_or_else(default_readers)
            .clamp(1, 64),
        chunk_bytes: var("SPACEMAN_MFT_CHUNK_KB")
            .map(|kb| (kb * 1024).next_multiple_of(ALIGNMENT).max(ALIGNMENT))
            .unwrap_or(CHUNK_BYTES),
        buffered: std::env::var_os("SPACEMAN_MFT_BUFFERED").is_some(),
        use_bitmap: std::env::var_os("SPACEMAN_MFT_NO_BITMAP").is_none(),
    }
}

fn default_readers() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, MAX_READERS)
}

#[derive(Debug)]
pub enum MftUnavailable {
    // only whole volumes are read through the MFT
    NotVolumeRoot,
    // the raw volume can only be opened as administrator
    AccessDenied,
    NotNtfs,
    Failed(String),
}

impl std::fmt::Display for MftUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MftUnavailable::NotVolumeRoot => write!(f, "not the root of a volume"),
            MftUnavailable::AccessDenied => write!(f, "administrator rights needed"),
            MftUnavailable::NotNtfs => write!(f, "not an NTFS volume"),
            MftUnavailable::Failed(reason) => write!(f, "{reason}"),
        }
    }
}

// "C:", "C:\" or "C:/" -> 'C'
pub fn volume_letter(path: &str) -> Option<char> {
    let mut chars = path.chars();
    let letter = chars.next().filter(|c| c.is_ascii_alphabetic())?;
    if chars.next() != Some(':') {
        return None;
    }
    match chars.as_str() {
        "" | "\\" | "/" => Some(letter.to_ascii_uppercase()),
        _ => None,
    }
}

// starts the MFT scan in the background, or explains why it cannot be used
pub fn start(directory: &str, shared: ScanShared) -> Result<(), MftUnavailable> {
    let letter = volume_letter(directory).ok_or(MftUnavailable::NotVolumeRoot)?;
    let tuning = tuning();
    let open_start = Instant::now();
    let mut volume = Volume::open(letter, &tuning)?;
    let step = Instant::now();
    let chunks = volume.chunks();
    volume.setup.push(("plan reads", step.elapsed()));
    let total_bytes: u64 = chunks.iter().map(|c| c.bytes as u64).sum();
    if cfg!(test) {
        println!(
            "MFT open={:?} extents={} chunks={} read_mb={} of {} readers={} chunk_kb={} buffered={} bitmap={}",
            open_start.elapsed(),
            volume.extents.len(),
            chunks.len(),
            total_bytes >> 20,
            volume.mft_valid_bytes >> 20,
            tuning.readers,
            tuning.chunk_bytes >> 10,
            tuning.buffered,
            volume.bitmap.is_some()
        );
        println!("MFT setup {:?}", volume.setup);
    }
    let total_records = volume.total_records();
    let volume = Arc::new(volume);
    let next_chunk = Arc::new(AtomicUsize::new(0));
    let chunks = Arc::new(chunks);

    let (tx, rx) = mpsc::channel();
    for _ in 0..tuning.readers {
        let volume = volume.clone();
        let chunks = chunks.clone();
        let next_chunk = next_chunk.clone();
        let tx: Sender<ChunkResult> = tx.clone();
        let shared = shared.clone();
        thread::spawn(move || reader(volume, chunks, next_chunk, tx, shared));
    }
    drop(tx);

    let chunk_count = chunks.len();
    thread::spawn(move || build_tree(rx, shared, total_records, total_bytes, chunk_count));
    Ok(())
}

// restarts this program elevated, scanning the same path; false if the user declined
pub fn relaunch_as_admin(path: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // a trailing backslash would escape the closing quote
    let argument = if path.ends_with('\\') {
        format!("\"{path}\\\"")
    } else {
        format!("\"{path}\"")
    };
    let wide = |s: &OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let operation = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let parameters = wide(OsStr::new(&argument));
    // SAFETY: all strings are NUL terminated and outlive the call
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    result > 32
}

#[derive(Debug, Clone, Copy)]
struct Extent {
    vcn: u64,
    lcn: u64,
    clusters: u64,
}

struct Chunk {
    volume_offset: u64,
    bytes: usize,
    first_record: u64,
}

struct Volume {
    file: File,
    sector: usize,
    cluster: u64,
    record_size: usize,
    mft_valid_bytes: u64,
    extents: Vec<Extent>,
    // one bit per record, set when the record is in use
    bitmap: Option<Vec<u8>>,
    chunk_bytes: usize,
    // how long each setup step took, for the benchmark output
    setup: Vec<(&'static str, Duration)>,
}

fn open_volume_handle(letter: char, buffered: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(if buffered { 0 } else { FILE_FLAG_NO_BUFFERING })
        .open(format!(r"\\.\{letter}:"))
}

fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

impl Volume {
    fn open(letter: char, tuning: &Tuning) -> Result<Volume, MftUnavailable> {
        let step = Instant::now();
        let file = open_volume_handle(letter, tuning.buffered).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => MftUnavailable::AccessDenied,
            _ => MftUnavailable::Failed(e.to_string()),
        })?;
        let mut setup = vec![("handle", step.elapsed())];
        let step = Instant::now();

        let mut data = [0u8; 128];
        let mut returned = 0u32;
        // SAFETY: the handle is open for the whole call and the output buffer
        // is valid for the size passed
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_NTFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                data.as_mut_ptr().cast(),
                data.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || returned < 72 {
            return Err(MftUnavailable::NotNtfs);
        }
        setup.push(("volume data", step.elapsed()));
        let field32 = |off| u32_at(&data, off).unwrap_or(0) as u64;
        let field64 = |off| u64_at(&data, off).unwrap_or(0);
        let sector = field32(40) as usize;
        let cluster = field32(44);
        let record_size = field32(48) as usize;
        let mft_valid_bytes = field64(56);
        let mft_start_lcn = field64(64);

        let sane = sector.is_power_of_two()
            && sector <= ALIGNMENT
            && cluster as usize >= sector
            && record_size >= 512
            && record_size.is_power_of_two()
            // every chunk then starts on a record boundary
            && cluster as usize % record_size == 0;
        if !sane {
            return Err(MftUnavailable::Failed(format!(
                "unsupported geometry: sector {sector}, cluster {cluster}, record {record_size}"
            )));
        }

        let mut volume = Volume {
            file,
            sector,
            cluster,
            record_size,
            mft_valid_bytes,
            extents: vec![],
            bitmap: None,
            chunk_bytes: tuning.chunk_bytes,
            setup,
        };
        volume.load_mft_extents(mft_start_lcn)?;
        if !tuning.use_bitmap {
            volume.bitmap = None;
        }
        Ok(volume)
    }

    fn total_records(&self) -> u64 {
        self.mft_valid_bytes / self.record_size as u64
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        read_exact_at(&self.file, buf, offset)
    }

    // reads one MFT record, with the update sequence fixups applied
    fn read_record_at(&self, volume_offset: u64) -> Result<Vec<u8>, MftUnavailable> {
        let sector = self.sector as u64;
        let start = volume_offset / sector * sector;
        let end = (volume_offset + self.record_size as u64).div_ceil(sector) * sector;
        let mut buf = AlignedBuf::new((end - start) as usize);
        self.read_exact_at(buf.as_mut_slice(), start)
            .map_err(|e| MftUnavailable::Failed(format!("reading MFT record: {e}")))?;
        let from = (volume_offset - start) as usize;
        let mut record = buf.as_mut_slice()[from..from + self.record_size].to_vec();
        if &record[0..4] != b"FILE" || !apply_fixups(&mut record) {
            return Err(MftUnavailable::Failed("corrupt MFT record".into()));
        }
        Ok(record)
    }

    fn record_offset(&self, record: u64) -> Option<u64> {
        let byte = record * self.record_size as u64;
        let vcn = byte / self.cluster;
        self.extents
            .iter()
            .find(|e| vcn >= e.vcn && vcn < e.vcn + e.clusters)
            .map(|e| (e.lcn + vcn - e.vcn) * self.cluster + byte % self.cluster)
    }

    // the MFT is a file itself: record 0 describes where its pieces are
    fn load_mft_extents(&mut self, mft_start_lcn: u64) -> Result<(), MftUnavailable> {
        let step = Instant::now();
        let record0 = self.read_record_at(mft_start_lcn * self.cluster)?;
        let mut extension_records = vec![];
        let mut bitmap_attr = None;
        for attr in attributes(&record0) {
            let attr_type = u32_at(attr, 0).unwrap_or(0);
            if attr_type == ATTR_DATA && attr.get(9) == Some(&0) {
                decode_data_runs(attr, &mut self.extents)?;
            } else if attr_type == ATTR_BITMAP && attr.get(9) == Some(&0) {
                bitmap_attr = Some(attr.to_vec());
            } else if attr_type == ATTR_ATTRIBUTE_LIST {
                extension_records = if attr.get(8) == Some(&0) {
                    mft_extensions(resident_value(attr).unwrap_or(&[]))
                } else {
                    // a very fragmented MFT keeps even its attribute list outside the record
                    mft_extensions(&self.read_non_resident(attr)?)
                };
            }
        }
        self.setup.push(("record 0", step.elapsed()));
        let step = Instant::now();
        // a fragmented MFT continues its run list in extension records; each one
        // must lie in a part of the MFT that is already mapped, so map in rounds
        while !extension_records.is_empty() {
            let mut unresolved = vec![];
            let mut progressed = false;
            for record in extension_records {
                let Some(offset) = self.record_offset(record) else {
                    unresolved.push(record);
                    continue;
                };
                let extension = self.read_record_at(offset)?;
                for attr in attributes(&extension) {
                    let attr_type = u32_at(attr, 0).unwrap_or(0);
                    if attr_type == ATTR_DATA && attr.get(9) == Some(&0) {
                        decode_data_runs(attr, &mut self.extents)?;
                    } else if attr_type == ATTR_BITMAP && attr.get(9) == Some(&0) {
                        bitmap_attr = Some(attr.to_vec());
                    }
                }
                progressed = true;
            }
            if !progressed {
                return Err(MftUnavailable::Failed(
                    "MFT extension record outside the mapped MFT".into(),
                ));
            }
            extension_records = unresolved;
        }
        self.extents.sort_by_key(|e| e.vcn);
        let mapped = self.extents.iter().map(|e| e.clusters).sum::<u64>() * self.cluster;
        if self.extents.is_empty() || mapped < self.mft_valid_bytes {
            return Err(MftUnavailable::Failed("incomplete MFT run list".into()));
        }
        self.setup.push(("extension records", step.elapsed()));
        let step = Instant::now();
        // without the bitmap every record is read, which is only slower
        self.bitmap = bitmap_attr.and_then(|attr| {
            let bits = if attr.get(8) == Some(&0) {
                resident_value(&attr).map(|v| v.to_vec())
            } else {
                self.read_non_resident(&attr).ok()
            }?;
            (bits.len() as u64 * 8 >= self.total_records()).then_some(bits)
        });
        self.setup.push(("bitmap", step.elapsed()));
        Ok(())
    }

    fn record_in_use(&self, record: u64) -> bool {
        self.bitmap.as_ref().is_none_or(|bits| {
            bits.get((record / 8) as usize)
                .is_none_or(|byte| byte >> (record % 8) & 1 == 1)
        })
    }

    // the content of a small non-resident attribute, read from its clusters
    fn read_non_resident(&self, attr: &[u8]) -> Result<Vec<u8>, MftUnavailable> {
        const MAX_BYTES: u64 = 64 * 1024 * 1024;
        let size = u64_at(attr, 0x30).unwrap_or(0);
        if size > MAX_BYTES {
            return Err(MftUnavailable::Failed(
                "MFT attribute list too large".into(),
            ));
        }
        let mut extents = vec![];
        decode_data_runs(attr, &mut extents)?;
        extents.sort_by_key(|e| e.vcn);
        let mut data = vec![0u8; size as usize];
        for extent in extents {
            let start = extent.vcn * self.cluster;
            if start >= size {
                continue;
            }
            let bytes = extent.clusters * self.cluster;
            if bytes > MAX_BYTES {
                return Err(MftUnavailable::Failed("corrupt MFT attribute list".into()));
            }
            let mut buf = AlignedBuf::new(bytes as usize);
            self.read_exact_at(buf.as_mut_slice(), extent.lcn * self.cluster)
                .map_err(|e| MftUnavailable::Failed(format!("reading MFT attribute list: {e}")))?;
            let take = bytes.min(size - start) as usize;
            data[start as usize..start as usize + take]
                .copy_from_slice(&buf.as_mut_slice()[..take]);
        }
        Ok(data)
    }

    // the reads covering every record in use; a read never crosses an extent
    fn chunks(&self) -> Vec<Chunk> {
        let mut chunks = vec![];
        let sector = self.sector as u64;
        let record_size = self.record_size as u64;
        let total_records = self.total_records();
        let gap_records = (MERGE_GAP_BYTES / record_size).max(1);
        for extent in &self.extents {
            let extent_start = extent.vcn * self.cluster;
            let extent_end = (extent.vcn + extent.clusters) * self.cluster;
            if extent_start >= self.mft_valid_bytes {
                break;
            }
            let last = (extent_end.min(self.mft_valid_bytes) / record_size).min(total_records);
            let mut record = extent_start / record_size;
            while record < last {
                if !self.record_in_use(record) {
                    record += 1;
                    continue;
                }
                // a span of used records, bridging short gaps
                let span_start = record;
                let mut span_end = record + 1;
                record += 1;
                while record < last && record - span_end <= gap_records {
                    if self.record_in_use(record) {
                        span_end = record + 1;
                    }
                    record += 1;
                }
                // sector aligned for unbuffered reads, extents are whole clusters
                let start = span_start * record_size / sector * sector;
                let end = ((span_end * record_size).div_ceil(sector) * sector).min(extent_end);
                let mut offset = start;
                while offset < end {
                    let len = (end - offset).min(self.chunk_bytes as u64);
                    chunks.push(Chunk {
                        volume_offset: extent.lcn * self.cluster + (offset - extent_start),
                        bytes: len as usize,
                        first_record: offset / record_size,
                    });
                    offset += len;
                }
            }
        }
        chunks
    }
}

// buffer aligned for unbuffered volume reads
struct AlignedBuf {
    raw: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let raw = vec![0u8; len + ALIGNMENT];
        let offset = raw.as_ptr().align_offset(ALIGNMENT);
        AlignedBuf { raw, offset, len }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.raw[self.offset..self.offset + self.len]
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

// NTFS replaces the last two bytes of every sector of a record with a check
// value and keeps the originals in the update sequence array
fn apply_fixups(record: &mut [u8]) -> bool {
    let (Some(usa_offset), Some(usa_count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let (usa_offset, usa_count) = (usa_offset as usize, usa_count as usize);
    if usa_count < 2 || usa_offset + usa_count * 2 > record.len() {
        return false;
    }
    let stride = record.len() / (usa_count - 1);
    if stride < 2 || stride * (usa_count - 1) != record.len() {
        return false;
    }
    let check = &record[usa_offset..usa_offset + 2];
    let check = [check[0], check[1]];
    for i in 1..usa_count {
        let end = i * stride - 2;
        if record[end..end + 2] != check {
            return false;
        }
        let original = usa_offset + i * 2;
        record.copy_within(original..original + 2, end);
    }
    true
}

// the attributes of a record (after fixups), each as its own slice
fn attributes(record: &[u8]) -> impl Iterator<Item = &[u8]> {
    let used = (u32_at(record, 24).unwrap_or(0) as usize).min(record.len());
    let mut offset = u16_at(record, 20).unwrap_or(u16::MAX) as usize;
    std::iter::from_fn(move || {
        let attr_type = u32_at(record, offset)?;
        let len = u32_at(record, offset + 4)? as usize;
        if attr_type == ATTR_END || len < 16 || offset + len > used {
            return None;
        }
        let attr = &record[offset..offset + len];
        offset += len;
        Some(attr)
    })
}

fn resident_value(attr: &[u8]) -> Option<&[u8]> {
    let len = u32_at(attr, 0x10)? as usize;
    let offset = u16_at(attr, 0x14)? as usize;
    attr.get(offset..offset + len)
}

// record numbers holding further pieces of the MFT's own $DATA or $BITMAP
fn mft_extensions(list: &[u8]) -> Vec<u64> {
    let mut records = vec![];
    let mut offset = 0;
    while let (Some(attr_type), Some(len)) = (u32_at(list, offset), u16_at(list, offset + 4)) {
        if len < 0x1A {
            break;
        }
        let name_len = list.get(offset + 6).copied().unwrap_or(1);
        let record = u64_at(list, offset + 0x10).unwrap_or(0) & REF_MASK;
        let wanted = attr_type == ATTR_DATA || attr_type == ATTR_BITMAP;
        if wanted && name_len == 0 && record != 0 && !records.contains(&record) {
            records.push(record);
        }
        offset += len as usize;
    }
    records
}

fn decode_data_runs(attr: &[u8], extents: &mut Vec<Extent>) -> Result<(), MftUnavailable> {
    let bad = || MftUnavailable::Failed("corrupt MFT run list".into());
    if attr.get(8) == Some(&0) {
        return Err(bad());
    }
    let mut vcn = u64_at(attr, 0x10).ok_or_else(bad)?;
    let runs_offset = u16_at(attr, 0x20).ok_or_else(bad)? as usize;
    let runs = attr.get(runs_offset..).ok_or_else(bad)?;
    let mut i = 0;
    let mut lcn: i64 = 0;
    while let Some(&header) = runs.get(i) {
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let offset_size = (header >> 4) as usize;
        i += 1;
        if len_size == 0
            || len_size > 8
            || offset_size > 8
            || i + len_size + offset_size > runs.len()
        {
            return Err(bad());
        }
        let mut clusters = 0u64;
        for (k, byte) in runs[i..i + len_size].iter().enumerate() {
            clusters |= (*byte as u64) << (8 * k);
        }
        i += len_size;
        if offset_size == 0 {
            // sparse run, nothing on disk
            vcn += clusters;
            continue;
        }
        let mut delta = 0i64;
        for (k, byte) in runs[i..i + offset_size].iter().enumerate() {
            delta |= (*byte as i64) << (8 * k);
        }
        // sign extend
        let shift = 64 - 8 * offset_size as u32;
        delta = (delta << shift) >> shift;
        i += offset_size;
        lcn += delta;
        if lcn < 0 {
            return Err(bad());
        }
        extents.push(Extent {
            vcn,
            lcn: lcn as u64,
            clusters,
        });
        vcn += clusters;
    }
    Ok(())
}

const PARSED_EXTENSION: u8 = 1;
const PARSED_DIR: u8 = 2;
const PARSED_NAME: u8 = 4;
const PARSED_DATA: u8 = 8;

// what one in-use record contributes; extension records carry attributes
// that did not fit into their base record
#[derive(Debug, Clone, Copy)]
struct Parsed {
    record: u32,
    parent: u32,
    size: u64,
    name_start: u32,
    name_len: u32,
    parent_seq: u16,
    seq: u16,
    flags: u8,
}

struct ChunkResult {
    records: u64,
    bytes: u64,
    read_time: Duration,
    parse_time: Duration,
    parsed: Vec<Parsed>,
    // OsStr encoded bytes of all names, back to back
    names: Vec<u8>,
}

fn parse_record(
    record: &mut [u8],
    number: u32,
    out: &mut Vec<Parsed>,
    names: &mut Vec<u8>,
    utf16: &mut Vec<u16>,
) {
    if record.get(0..4) != Some(b"FILE") || !apply_fixups(record) {
        return;
    }
    let flags = u16_at(record, 22).unwrap_or(0);
    if flags & RECORD_IN_USE == 0 {
        return;
    }
    let seq = u16_at(record, 16).unwrap_or(0);
    let base = u64_at(record, 32).unwrap_or(0) & REF_MASK;
    let mut parsed = Parsed {
        record: number,
        parent: 0,
        size: 0,
        name_start: 0,
        name_len: 0,
        parent_seq: 0,
        seq,
        flags: 0,
    };
    if base != 0 {
        let Ok(base) = u32::try_from(base) else {
            return;
        };
        parsed.record = base;
        parsed.flags |= PARSED_EXTENSION;
    }
    if flags & RECORD_IS_DIRECTORY != 0 {
        parsed.flags |= PARSED_DIR;
    }

    for attr in attributes(record) {
        let attr_type = u32_at(attr, 0).unwrap_or(0);
        let non_resident = attr.get(8) != Some(&0);
        let attr_name_len = attr.get(9).copied().unwrap_or(1);
        if attr_type == ATTR_FILE_NAME && !non_resident && parsed.flags & PARSED_NAME == 0 {
            let Some(value) = resident_value(attr) else {
                continue;
            };
            let (Some(&len), Some(&namespace)) = (value.get(0x40), value.get(0x41)) else {
                continue;
            };
            // skip 8.3 short names, the long name comes in its own attribute
            if namespace == NAMESPACE_DOS {
                continue;
            }
            let Some(raw) = value.get(0x42..0x42 + 2 * len as usize) else {
                continue;
            };
            let parent_ref = u64_at(value, 0).unwrap_or(0);
            let Ok(parent) = u32::try_from(parent_ref & REF_MASK) else {
                continue;
            };
            utf16.clear();
            utf16.extend(
                raw.chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]])),
            );
            let name = OsString::from_wide(utf16);
            parsed.name_start = names.len() as u32;
            names.extend_from_slice(name.as_encoded_bytes());
            parsed.name_len = names.len() as u32 - parsed.name_start;
            parsed.parent = parent;
            parsed.parent_seq = (parent_ref >> 48) as u16;
            parsed.flags |= PARSED_NAME;
        } else if attr_type == ATTR_DATA && attr_name_len == 0 {
            // named data attributes are alternate data streams, not counted
            if non_resident {
                // only the first piece of a split attribute holds the size
                if u64_at(attr, 0x10) == Some(0) {
                    parsed.size = u64_at(attr, 0x30).unwrap_or(0);
                    parsed.flags |= PARSED_DATA;
                }
            } else {
                parsed.size = u32_at(attr, 0x10).unwrap_or(0) as u64;
                parsed.flags |= PARSED_DATA;
            }
        }
    }
    if parsed.flags & PARSED_EXTENSION == 0 || parsed.flags & (PARSED_NAME | PARSED_DATA) != 0 {
        out.push(parsed);
    }
}

fn reader(
    volume: Arc<Volume>,
    chunks: Arc<Vec<Chunk>>,
    next_chunk: Arc<AtomicUsize>,
    tx: Sender<ChunkResult>,
    shared: ScanShared,
) {
    let mut buf = AlignedBuf::new(volume.chunk_bytes);
    let mut utf16 = Vec::new();
    let total_records = volume.total_records();
    loop {
        if shared.terminated() {
            break;
        }
        let index = next_chunk.fetch_add(1, Ordering::SeqCst);
        let Some(chunk) = chunks.get(index) else {
            break;
        };
        let data = &mut buf.as_mut_slice()[..chunk.bytes];
        let mut result = ChunkResult {
            records: 0,
            bytes: chunk.bytes as u64,
            read_time: Duration::ZERO,
            parse_time: Duration::ZERO,
            parsed: Vec::with_capacity(chunk.bytes / volume.record_size),
            names: Vec::with_capacity(chunk.bytes / 16),
        };
        let read_start = Instant::now();
        let read = volume.read_exact_at(data, chunk.volume_offset);
        result.read_time = read_start.elapsed();
        let parse_start = Instant::now();
        match read {
            Ok(()) => {
                for (i, record) in data.chunks_exact_mut(volume.record_size).enumerate() {
                    let number = chunk.first_record + i as u64;
                    if number >= total_records {
                        break;
                    }
                    let Ok(number) = u32::try_from(number) else {
                        break;
                    };
                    result.records += 1;
                    parse_record(
                        record,
                        number,
                        &mut result.parsed,
                        &mut result.names,
                        &mut utf16,
                    );
                }
            }
            Err(e) => println!("Can't read MFT at {}: {}", chunk.volume_offset, e),
        }
        result.parse_time = parse_start.elapsed();
        if tx.send(result).is_err() {
            break;
        }
    }
}

// a record that cannot be inserted yet: its parent is not in the tree, or
// its base record or name has not been read yet
#[derive(Default)]
struct Pending {
    parent: u32,
    parent_seq: u16,
    name: Option<Box<[u8]>>,
    size: u64,
    base_seen: bool,
    is_dir: bool,
    waiting: bool,
}

struct Builder {
    // record number -> tree node, NO_NODE until inserted
    nodes: Vec<NodeID>,
    seqs: Vec<u16>,
    pending: HashMap<u32, Pending>,
    // parent record -> records waiting for it
    waiting: HashMap<u32, Vec<u32>>,
}

impl Builder {
    fn new(total_records: usize) -> Self {
        let mut nodes = vec![NO_NODE; total_records];
        if let Some(root) = nodes.get_mut(ROOT_RECORD as usize) {
            *root = 0;
        }
        Builder {
            nodes,
            seqs: vec![0; total_records],
            pending: HashMap::new(),
            waiting: HashMap::new(),
        }
    }

    fn node(&self, record: u32) -> NodeID {
        self.nodes.get(record as usize).copied().unwrap_or(NO_NODE)
    }

    fn apply(&mut self, tree: &mut Tree, parsed: &Parsed, names: &[u8]) {
        let record = parsed.record;
        if record as usize >= self.nodes.len() {
            return;
        }
        let name = (parsed.flags & PARSED_NAME != 0).then(|| {
            let start = parsed.name_start as usize;
            &names[start..start + parsed.name_len as usize]
        });
        let size = if parsed.flags & PARSED_DATA != 0 {
            parsed.size
        } else {
            0
        };

        if parsed.flags & PARSED_EXTENSION == 0 {
            self.seqs[record as usize] = parsed.seq;
            if record == ROOT_RECORD {
                return;
            }
            // fast path: most records can go straight into the tree
            if let Some(name) = name
                && !self.pending.contains_key(&record)
                && self.parent_ready(parsed.parent, parsed.parent_seq) == Some(true)
            {
                let parent_node = self.node(parsed.parent);
                // SAFETY: name is one whole as_encoded_bytes() output of the chunk arena
                let name = unsafe { OsStr::from_encoded_bytes_unchecked(name) };
                self.insert(
                    tree,
                    record,
                    parent_node,
                    name,
                    parsed.flags & PARSED_DIR != 0,
                    size,
                );
                return;
            }
            let entry = self.pending.entry(record).or_default();
            entry.base_seen = true;
            entry.is_dir = parsed.flags & PARSED_DIR != 0;
            entry.size += size;
            if entry.name.is_none()
                && let Some(name) = name
            {
                entry.name = Some(name.into());
                entry.parent = parsed.parent;
                entry.parent_seq = parsed.parent_seq;
            }
            self.try_insert(tree, record);
        } else {
            if size > 0 {
                let node = self.node(record);
                if node != NO_NODE {
                    tree.propagate_size(node, size);
                } else {
                    self.pending.entry(record).or_default().size += size;
                }
            }
            if let Some(name) = name
                && self.node(record) == NO_NODE
            {
                let entry = self.pending.entry(record).or_default();
                if entry.name.is_none() {
                    entry.name = Some(name.into());
                    entry.parent = parsed.parent;
                    entry.parent_seq = parsed.parent_seq;
                    self.try_insert(tree, record);
                }
            }
        }
    }

    // Some(true) parent in tree, Some(false) parent reused for another file, None not yet known
    fn parent_ready(&self, parent: u32, parent_seq: u16) -> Option<bool> {
        if self.node(parent) == NO_NODE {
            return None;
        }
        // the root's sequence number may not be read yet, it never changes
        Some(parent == ROOT_RECORD || self.seqs[parent as usize] == parent_seq)
    }

    fn insert(
        &mut self,
        tree: &mut Tree,
        record: u32,
        parent_node: NodeID,
        name: &OsStr,
        is_dir: bool,
        size: u64,
    ) {
        let node = tree.add_child(parent_node, name, !is_dir, size);
        tree.propagate_size(parent_node, size);
        self.nodes[record as usize] = node;
        if self.waiting.contains_key(&record) {
            self.release_waiting(tree, record);
        }
    }

    fn try_insert(&mut self, tree: &mut Tree, record: u32) {
        let Some(entry) = self.pending.get(&record) else {
            return;
        };
        if !entry.base_seen || entry.name.is_none() || entry.waiting {
            return;
        }
        let (parent, parent_seq) = (entry.parent, entry.parent_seq);
        if parent == record || parent as usize >= self.nodes.len() {
            self.pending.remove(&record);
            return;
        }
        match self.parent_ready(parent, parent_seq) {
            None => {
                if let Some(entry) = self.pending.get_mut(&record) {
                    entry.waiting = true;
                }
                self.waiting.entry(parent).or_default().push(record);
            }
            Some(false) => {
                // orphan of a deleted folder whose record was reused
                self.pending.remove(&record);
            }
            Some(true) => {
                let entry = self.pending.remove(&record).unwrap();
                let name = entry.name.unwrap();
                // SAFETY: the bytes were copied from one whole as_encoded_bytes() output
                let name = unsafe { OsStr::from_encoded_bytes_unchecked(&name) };
                let parent_node = self.node(parent);
                self.insert(tree, record, parent_node, name, entry.is_dir, entry.size);
            }
        }
    }

    // inserts everything that was waiting for this folder, iteratively since
    // whole subtrees can be waiting
    fn release_waiting(&mut self, tree: &mut Tree, record: u32) {
        let mut ready = vec![record];
        while let Some(parent) = ready.pop() {
            let Some(children) = self.waiting.remove(&parent) else {
                continue;
            };
            for child in children {
                let Some(entry) = self.pending.get(&child) else {
                    continue;
                };
                if self.parent_ready(parent, entry.parent_seq) != Some(true) {
                    self.pending.remove(&child);
                    continue;
                }
                let entry = self.pending.remove(&child).unwrap();
                let name = entry.name.unwrap();
                // SAFETY: the bytes were copied from one whole as_encoded_bytes() output
                let name = unsafe { OsStr::from_encoded_bytes_unchecked(&name) };
                let parent_node = self.node(parent);
                let node = tree.add_child(parent_node, name, !entry.is_dir, entry.size);
                tree.propagate_size(parent_node, entry.size);
                self.nodes[child as usize] = node;
                ready.push(child);
            }
        }
    }
}

fn build_tree(
    rx: Receiver<ChunkResult>,
    shared: ScanShared,
    total_records: u64,
    total_bytes: u64,
    chunk_count: usize,
) {
    let started = Instant::now();
    let mut builder = Builder::new(total_records as usize);
    let (mut read_time, mut parse_time, mut apply_time) =
        (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let mut chunks_done = 0;
    let mut records_done = 0u64;
    let mut bytes_done = 0u64;
    let mut batch: Vec<ChunkResult> = vec![];
    let mut window_start = Instant::now();
    let mut finished = false;

    while !finished && !shared.terminated() {
        let wait = BATCH_WINDOW.saturating_sub(window_start.elapsed());
        match rx.recv_timeout(wait.max(std::time::Duration::from_millis(1))) {
            Ok(result) => batch.push(result),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => finished = true,
        }
        while let Ok(result) = rx.try_recv() {
            batch.push(result);
        }
        if batch.is_empty() || (!finished && window_start.elapsed() < BATCH_WINDOW) {
            continue;
        }
        {
            let apply_start = Instant::now();
            let mut tree = shared.tree.lock().unwrap();
            if shared.terminated() {
                break;
            }
            for result in batch.drain(..) {
                read_time += result.read_time;
                parse_time += result.parse_time;
                for parsed in &result.parsed {
                    builder.apply(&mut tree, parsed, &result.names);
                }
                records_done += result.records;
                bytes_done += result.bytes;
                chunks_done += 1;
            }
            apply_time += apply_start.elapsed();
        }
        finished |= chunks_done == chunk_count;
        shared.set_progress(bytes_done as f64 / total_bytes.max(1) as f64);
        shared.update_signal.store(true, Ordering::SeqCst);
        window_start = Instant::now();
    }
    if cfg!(test) {
        println!(
            "MFT records={records_done} chunks={chunks_done} wall={:?} read(sum)={read_time:?} parse(sum)={parse_time:?} apply={apply_time:?} pending_left={} waiting_left={}",
            started.elapsed(),
            builder.pending.len(),
            builder.waiting.len()
        );
    }
    shared.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    // a 1024 byte record with fixups for 512 byte sectors
    fn record_with(attrs: &[Vec<u8>], flags: u16, base: u64) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // usa offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // usa count
        r[16..18].copy_from_slice(&7u16.to_le_bytes()); // sequence
        r[20..22].copy_from_slice(&0x38u16.to_le_bytes()); // first attribute
        r[22..24].copy_from_slice(&flags.to_le_bytes());
        r[32..40].copy_from_slice(&base.to_le_bytes());
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        r[24..28].copy_from_slice(&((off + 8) as u32).to_le_bytes());
        // fixups: original sector tails go into the array, the check value replaces them
        let usn = 0xABCDu16.to_le_bytes();
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 1..3 {
            let end = i * 512 - 2;
            let (a, b) = (r[end], r[end + 1]);
            r[0x30 + 2 * i] = a;
            r[0x30 + 2 * i + 1] = b;
            r[end..end + 2].copy_from_slice(&usn);
        }
        r
    }

    fn resident(attr_type: u32, name_len: u8, value: &[u8]) -> Vec<u8> {
        let len = (0x18 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&attr_type.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = name_len;
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x18..0x18 + value.len()].copy_from_slice(value);
        a
    }

    fn file_name(parent: u64, parent_seq: u16, namespace: u8, name: &str) -> Vec<u8> {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + utf16.len() * 2];
        v[0..8].copy_from_slice(&(parent | (parent_seq as u64) << 48).to_le_bytes());
        v[0x40] = utf16.len() as u8;
        v[0x41] = namespace;
        for (i, c) in utf16.iter().enumerate() {
            v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, 0, &v)
    }

    fn non_resident_data(lowest_vcn: u64, size: u64, runs: &[u8]) -> Vec<u8> {
        let len = (0x40 + runs.len() + 1).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[0x10..0x18].copy_from_slice(&lowest_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x40..0x40 + runs.len()].copy_from_slice(runs);
        a
    }

    fn parse(mut record: Vec<u8>, number: u32) -> (Vec<Parsed>, Vec<u8>) {
        let (mut out, mut names, mut utf16) = (vec![], vec![], vec![]);
        parse_record(&mut record, number, &mut out, &mut names, &mut utf16);
        (out, names)
    }

    #[test]
    fn volume_letters() {
        assert_eq!(volume_letter("c:\\"), Some('C'));
        assert_eq!(volume_letter("D:"), Some('D'));
        assert_eq!(volume_letter("E:/"), Some('E'));
        assert_eq!(volume_letter("C:\\Windows"), None);
        assert_eq!(volume_letter("\\\\server\\share"), None);
    }

    #[test]
    fn parses_file_record_with_long_name() {
        let record = record_with(
            &[
                file_name(42, 3, NAMESPACE_DOS, "LONGNA~1.TXT"),
                file_name(42, 3, 1, "long name ✓.txt"),
                resident(ATTR_DATA, 0, &[1, 2, 3, 4, 5]),
                resident(ATTR_DATA, 3, &[0; 40]), // alternate data stream, ignored
            ],
            RECORD_IN_USE,
            0,
        );
        let (out, names) = parse(record, 100);
        assert_eq!(out.len(), 1);
        let p = out[0];
        assert_eq!(
            (p.record, p.parent, p.parent_seq, p.seq, p.size),
            (100, 42, 3, 7, 5)
        );
        assert_eq!(p.flags, PARSED_NAME | PARSED_DATA);
        let name = unsafe { OsStr::from_encoded_bytes_unchecked(&names) };
        assert_eq!(name, "long name ✓.txt");
    }

    #[test]
    fn extension_record_size_goes_to_base() {
        let record = record_with(
            &[non_resident_data(0, 1 << 40, &[0x11, 0x10, 0x20, 0])],
            RECORD_IN_USE,
            77 | 2 << 48,
        );
        let (out, _) = parse(record, 500);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].record, out[0].size), (77, 1 << 40));
        assert_eq!(out[0].flags, PARSED_EXTENSION | PARSED_DATA);
        // a later piece of a split attribute does not carry the size
        let record = record_with(
            &[non_resident_data(16, 1 << 40, &[0x11, 0x10, 0x20, 0])],
            RECORD_IN_USE,
            77,
        );
        assert!(parse(record, 501).0.is_empty());
    }

    #[test]
    fn rejects_unused_and_torn_records() {
        let unused = record_with(&[file_name(5, 5, 1, "x")], 0, 0);
        assert!(parse(unused, 1).0.is_empty());
        let mut torn = record_with(&[file_name(5, 5, 1, "x")], RECORD_IN_USE, 0);
        torn[510] ^= 0xFF;
        assert!(parse(torn, 1).0.is_empty());
    }

    #[test]
    fn chunks_skip_unused_records_and_stay_in_extents() {
        // 1024 records of 1 KiB in two extents of 128 clusters (512 records) each
        let mut bitmap = vec![0u8; 128];
        let mut set = |r: usize| bitmap[r / 8] |= 1 << (r % 8);
        (0..10).for_each(&mut set); // records 0..10 used
        set(300); // gap of 289 records (> 256 KiB) before it: separate read
        set(500); // gap of 199 records: bridged
        set(511);
        set(512); // first record of the second extent: a new read
        let volume = Volume {
            file: tempfile::tempfile().unwrap(),
            setup: vec![],
            sector: 512,
            cluster: 4096,
            record_size: 1024,
            mft_valid_bytes: 1024 * 1024,
            extents: vec![
                Extent {
                    vcn: 0,
                    lcn: 1000,
                    clusters: 128,
                },
                Extent {
                    vcn: 128,
                    lcn: 5000,
                    clusters: 128,
                },
            ],
            bitmap: Some(bitmap),
            chunk_bytes: 64 * 1024,
        };
        let got: Vec<_> = volume
            .chunks()
            .iter()
            .map(|c| (c.volume_offset, c.bytes, c.first_record))
            .collect();
        assert_eq!(
            got,
            vec![
                (1000 * 4096, 10 * 1024, 0),
                // records 300..512, split at the 64 KiB chunk size
                (1000 * 4096 + 300 * 1024, 64 * 1024, 300),
                (1000 * 4096 + 364 * 1024, 64 * 1024, 364),
                (1000 * 4096 + 428 * 1024, 64 * 1024, 428),
                (1000 * 4096 + 492 * 1024, 20 * 1024, 492),
                (5000 * 4096, 1024, 512),
            ]
        );
        // without a bitmap everything is read
        let all = Volume {
            bitmap: None,
            ..volume
        };
        let bytes: usize = all.chunks().iter().map(|c| c.bytes).sum();
        assert_eq!(bytes, 1024 * 1024);
    }

    #[test]
    fn decodes_run_lists() {
        // 0x20 clusters at 0x1000, then 0x10 clusters 0x100 before that, a sparse run, then +0x30
        let runs = [
            0x21, 0x20, 0x00, 0x10, 0x21, 0x10, 0x00, 0xFF, 0x01, 0x08, 0x11, 0x04, 0x30, 0x00,
        ];
        let attr = non_resident_data(0, 0, &runs);
        let mut extents = vec![];
        decode_data_runs(&attr, &mut extents).unwrap();
        let got: Vec<_> = extents.iter().map(|e| (e.vcn, e.lcn, e.clusters)).collect();
        assert_eq!(
            got,
            vec![
                (0, 0x1000, 0x20),
                (0x20, 0x0F00, 0x10),
                (0x38, 0x0F30, 0x04)
            ]
        );
    }

    #[test]
    fn builder_handles_children_before_parents() {
        let mut tree = Tree::new("C:\\");
        let mut builder = Builder::new(64);
        let mut names = vec![];
        let mut add = |name: &str| {
            let start = names.len() as u32;
            names.extend_from_slice(name.as_bytes());
            (start, name.len() as u32)
        };
        let (a, b, c) = (add("dir"), add("file"), add("stale"));
        let rec = |record, parent, parent_seq, seq, flags, size, name: (u32, u32)| Parsed {
            record,
            parent,
            size,
            name_start: name.0,
            name_len: name.1,
            parent_seq,
            seq,
            flags,
        };
        let parsed = [
            // file 20 in folder 10, read before the folder itself
            rec(20, 10, 1, 1, PARSED_NAME | PARSED_DATA, 100, b),
            // its size continues in an extension record
            rec(20, 0, 0, 0, PARSED_EXTENSION | PARSED_DATA, 50, (0, 0)),
            // points at folder 10 with an old sequence number: orphan
            rec(21, 10, 9, 1, PARSED_NAME | PARSED_DATA, 7, c),
            rec(10, ROOT_RECORD, 5, 1, PARSED_NAME | PARSED_DIR, 0, a),
        ];
        for p in &parsed {
            builder.apply(&mut tree, p, &names);
        }
        assert_eq!(tree.get_elem(0).size, 150);
        let dir = builder.node(10);
        assert_eq!(tree.name(dir), "dir");
        let files: Vec<_> = tree
            .children(dir)
            .map(|n| tree.name(n).into_owned())
            .collect();
        assert_eq!(files, vec!["file"]);
        assert_eq!(builder.node(21), NO_NODE);
        assert_eq!(
            tree.path_of(builder.node(20)),
            std::path::PathBuf::from("C:\\dir\\file")
        );
    }
}
