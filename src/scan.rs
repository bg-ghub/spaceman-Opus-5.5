use crate::types::{NodeID, Tree};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

// results are collected outside the tree lock and applied in one short
// critical section per window, so the UI never waits on the scan for long
const BATCH_WINDOW: Duration = Duration::from_millis(15);
const MAX_WORKERS: usize = 16;
const NO_TOP: u32 = u32::MAX;

#[derive(Debug)]
pub struct Scan {
    pub path: String,
    pub tree_mutex: Arc<Mutex<Tree>>,
    pub complete: Arc<AtomicBool>,
    pub update_signal: Arc<AtomicBool>,
    terminate_signal: Arc<AtomicBool>,
    queue: Arc<JobQueue>,
    progress_bits: Arc<AtomicU64>,
}

impl Scan {
    pub fn new(directory: &str) -> Self {
        let path = directory.to_string();
        let tree_mutex = Arc::new(Mutex::new(Tree::new(directory)));
        let terminate_signal = Arc::new(AtomicBool::new(false));
        let update_signal = Arc::new(AtomicBool::new(true));
        let complete = Arc::new(AtomicBool::new(false));
        let progress_bits = Arc::new(AtomicU64::new(0f64.to_bits()));
        let queue = Arc::new(JobQueue::default());
        let root_device = fs::metadata(directory).ok().and_then(|m| device_of(&m));

        let (tx, rx) = mpsc::channel();
        let workers = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, MAX_WORKERS);
        for _ in 0..workers {
            let queue = queue.clone();
            let tx: Sender<Listing> = tx.clone();
            let terminate = terminate_signal.clone();
            thread::spawn(move || worker(queue, tx, terminate, root_device));
        }
        drop(tx);

        queue.push_all(vec![Job {
            node: 0,
            path: PathBuf::from(directory),
            top: NO_TOP,
        }]);

        let consumer = Consumer {
            tree: tree_mutex.clone(),
            rx,
            queue: queue.clone(),
            update_signal: update_signal.clone(),
            terminate: terminate_signal.clone(),
            complete: complete.clone(),
            progress_bits: progress_bits.clone(),
            workers,
        };
        thread::spawn(move || consumer.run());

        Scan {
            path,
            tree_mutex,
            complete,
            update_signal,
            terminate_signal,
            queue,
            progress_bits,
        }
    }
    pub fn progress(&self) -> f64 {
        if self.complete.load(Ordering::SeqCst) {
            1.0
        } else {
            f64::from_bits(self.progress_bits.load(Ordering::Relaxed)) * 0.99
        }
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        // this will signal to the scan threads to exit after the Scan struct is out of scope
        self.complete.store(true, Ordering::SeqCst);
        self.terminate_signal.store(true, Ordering::SeqCst);
        self.queue.wake_all();
        // the consumer checks the terminate signal while holding this lock,
        // so it never applies a batch to the cleared tree
        self.tree_mutex.lock().unwrap().clear();
    }
}

#[cfg(unix)]
fn device_of(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::prelude::MetadataExt;
    Some(metadata.dev())
}

#[cfg(not(unix))]
fn device_of(_metadata: &fs::Metadata) -> Option<u64> {
    None
}

#[derive(Debug)]
struct Job {
    node: NodeID,
    path: PathBuf,
    // index of the top level folder this job belongs to, used for progress
    top: u32,
}

#[derive(Debug, Default)]
struct JobQueue {
    jobs: Mutex<VecDeque<Job>>,
    available: Condvar,
}

impl JobQueue {
    fn push_all(&self, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        self.jobs.lock().unwrap().extend(jobs);
        self.available.notify_all();
    }
    fn pop(&self, terminate: &AtomicBool) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        loop {
            if terminate.load(Ordering::SeqCst) {
                return None;
            }
            // breadth first, so every visible folder keeps growing while scanning
            if let Some(job) = jobs.pop_front() {
                return Some(job);
            }
            jobs = self
                .available
                .wait_timeout(jobs, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
    fn len(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }
    fn wake_all(&self) {
        self.available.notify_all();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    Dir,
    // a directory on another device, shown but not entered
    ForeignDir,
}

struct Entry {
    name_len: u16,
    kind: Kind,
    size: u64,
}

struct Listing {
    job: Job,
    // OsStr encoded bytes of all entry names, back to back
    names: Vec<u8>,
    entries: Vec<Entry>,
}

fn worker(
    queue: Arc<JobQueue>,
    tx: Sender<Listing>,
    terminate: Arc<AtomicBool>,
    root_device: Option<u64>,
) {
    while let Some(job) = queue.pop(&terminate) {
        let (names, entries) = list_dir(&job.path, root_device);
        if tx.send(Listing { job, names, entries }).is_err() {
            break;
        }
    }
}

fn list_dir(path: &Path, root_device: Option<u64>) -> (Vec<u8>, Vec<Entry>) {
    let mut names = Vec::new();
    let mut entries = Vec::new();
    let read_dir = match fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(e) => {
            println!("Can't read {}: {}", path.display(), e);
            return (names, entries);
        }
    };
    for dir_entry in read_dir.flatten() {
        let Ok(file_type) = dir_entry.file_type() else {
            continue;
        };
        // on Windows this comes from the directory listing itself, no extra syscall
        let metadata = dir_entry.metadata().ok();
        let size = metadata.as_ref().map_or(0, |m| m.len());
        // symlinks and junctions are not directories here, so they are never followed
        let kind = if !file_type.is_dir() {
            Kind::File
        } else if root_device.is_some()
            && metadata.as_ref().and_then(device_of) != root_device
        {
            Kind::ForeignDir
        } else {
            Kind::Dir
        };
        let file_name = dir_entry.file_name();
        let bytes = file_name.as_encoded_bytes();
        let name_len = if bytes.len() <= u16::MAX as usize {
            names.extend_from_slice(bytes);
            bytes.len() as u16
        } else {
            0
        };
        entries.push(Entry {
            name_len,
            kind,
            size,
        });
    }
    (names, entries)
}

// counts finished top level entries; a folder is finished when no job of its
// subtree is outstanding anymore
#[derive(Default)]
struct TopProgress {
    pending: Vec<u32>,
    total: usize,
    done: usize,
}

impl TopProgress {
    fn add_finished(&mut self) {
        self.total += 1;
        self.done += 1;
    }
    fn add_folder(&mut self) -> u32 {
        self.total += 1;
        self.pending.push(1);
        (self.pending.len() - 1) as u32
    }
    fn job_added(&mut self, top: u32) {
        self.pending[top as usize] += 1;
    }
    fn job_finished(&mut self, top: u32) {
        let pending = &mut self.pending[top as usize];
        *pending -= 1;
        if *pending == 0 {
            self.done += 1;
        }
    }
    fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.done as f64 / self.total as f64
        }
    }
}

struct Consumer {
    tree: Arc<Mutex<Tree>>,
    rx: Receiver<Listing>,
    queue: Arc<JobQueue>,
    update_signal: Arc<AtomicBool>,
    terminate: Arc<AtomicBool>,
    complete: Arc<AtomicBool>,
    progress_bits: Arc<AtomicU64>,
    workers: usize,
}

impl Consumer {
    fn run(self) {
        let mut pending: usize = 1;
        let mut progress = TopProgress::default();
        let mut batch: Vec<Listing> = Vec::new();
        let mut window_start = Instant::now();

        while pending > 0 && !self.terminate.load(Ordering::SeqCst) {
            let wait = BATCH_WINDOW.saturating_sub(window_start.elapsed());
            match self.rx.recv_timeout(wait.max(Duration::from_millis(1))) {
                Ok(listing) => batch.push(listing),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(listing) = self.rx.try_recv() {
                batch.push(listing);
            }
            // apply early when workers are about to run out of jobs
            let workers_starving = self.queue.len() < self.workers;
            if batch.is_empty() || (!workers_starving && window_start.elapsed() < BATCH_WINDOW) {
                continue;
            }

            let mut new_jobs = Vec::new();
            {
                let mut tree = self.tree.lock().unwrap();
                if self.terminate.load(Ordering::SeqCst) {
                    break;
                }
                for listing in batch.drain(..) {
                    let created = apply_listing(&mut tree, listing, &mut new_jobs, &mut progress);
                    pending = pending + created - 1;
                }
            }
            self.queue.push_all(new_jobs);
            self.progress_bits
                .store(progress.fraction().to_bits(), Ordering::Relaxed);
            self.update_signal.store(true, Ordering::SeqCst);
            window_start = Instant::now();
        }
        self.complete.store(true, Ordering::SeqCst);
        self.update_signal.store(true, Ordering::SeqCst);
        self.terminate.store(true, Ordering::SeqCst);
        self.queue.wake_all();
    }
}

// returns the number of jobs created
fn apply_listing(
    tree: &mut Tree,
    listing: Listing,
    new_jobs: &mut Vec<Job>,
    progress: &mut TopProgress,
) -> usize {
    let Listing {
        job,
        names,
        entries,
    } = listing;
    let is_root = job.top == NO_TOP;
    let mut offset = 0;
    let mut total_size = 0;
    let mut created = 0;
    for entry in entries {
        let end = offset + entry.name_len as usize;
        // SAFETY: names holds whole as_encoded_bytes() outputs back to back and
        // name_len is the exact length of one of them (or 0)
        let name = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&names[offset..end]) };
        offset = end;
        let id = tree.add_child(job.node, name, entry.kind == Kind::File, entry.size);
        total_size += entry.size;
        if entry.kind == Kind::Dir {
            let top = if is_root {
                progress.add_folder()
            } else {
                progress.job_added(job.top);
                job.top
            };
            new_jobs.push(Job {
                node: id,
                path: job.path.join(name),
                top,
            });
            created += 1;
        } else if is_root {
            progress.add_finished();
        }
    }
    tree.propagate_size(job.node, total_size);
    if !is_root {
        progress.job_finished(job.top);
    }
    created
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::squarify::compute_gui_nodes;
    use crate::types::Rectangle;

    // cargo test --release bench_scan -- --ignored --nocapture
    // with SPACEMAN_BENCH_PATH set to the folder to scan
    #[test]
    #[ignore]
    fn bench_scan() {
        let path = std::env::var("SPACEMAN_BENCH_PATH").unwrap_or_else(|_| ".".into());
        let start = Instant::now();
        let scan = Scan::new(&path);
        while !scan.complete.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
        let scan_time = start.elapsed();
        let tree = scan.tree_mutex.lock().unwrap();
        let bound = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let start = Instant::now();
        let gui_nodes = compute_gui_nodes(&tree, 0, bound, 20.0);
        let layout_time = start.elapsed();
        println!(
            "BENCH path={} nodes={} bytes={} scan={:?} layout={:?} boxes={} tree_heap_mb={:.1}",
            path,
            tree.len(),
            tree.get_elem(0).size,
            scan_time,
            layout_time,
            gui_nodes.len(),
            tree.heap_bytes() as f64 / (1024.0 * 1024.0)
        );
    }

    #[test]
    fn scan_matches_directory_contents() {
        let dir = std::env::temp_dir().join(format!("spaceman-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("one.bin"), vec![0u8; 1000]).unwrap();
        fs::write(dir.join("a/two.bin"), vec![0u8; 300]).unwrap();
        fs::write(dir.join("a/b/three.bin"), vec![0u8; 20]).unwrap();
        fs::write(dir.join("a/b/empty"), b"").unwrap();

        let scan = Scan::new(dir.to_str().unwrap());
        while !scan.complete.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
        {
            let tree = scan.tree_mutex.lock().unwrap();
            // directories count their own metadata size on unix, so compare file bytes only
            let mut file_bytes = 0;
            let mut stack = vec![0];
            let mut names = vec![];
            while let Some(id) = stack.pop() {
                let node = tree.get_elem(id);
                if node.is_file {
                    file_bytes += node.size;
                    names.push(tree.path_of(id));
                }
                stack.extend(tree.children(id));
            }
            assert_eq!(file_bytes, 1320);
            assert_eq!(tree.len(), 7);
            assert!(names.contains(&dir.join("a").join("b").join("three.bin")));
            assert_eq!(scan.progress(), 1.0);
        }
        drop(scan);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalidate_detaches_subtree() {
        let mut tree = Tree::new("root");
        let a = tree.add_child(0, "a".as_ref(), false, 0);
        let f = tree.add_child(a, "f".as_ref(), true, 10);
        tree.propagate_size(a, 10);
        let g = tree.add_child(0, "g".as_ref(), true, 5);
        tree.propagate_size(0, 5);
        assert_eq!(tree.get_elem(0).size, 15);
        tree.invalidate_elem(a);
        assert_eq!(tree.get_elem(0).size, 5);
        assert_eq!(tree.children(0).collect::<Vec<_>>(), vec![g]);
        // late scan results for the removed folder must not reach the root
        tree.propagate_size(f, 7);
        assert_eq!(tree.get_elem(0).size, 5);
    }

    /// Regression test for when we introduced time based updates.
    /// We ended up missing everything in small folders or the last few elements in a folder,
    /// as the scan completed before the first flush happened.
    #[test]
    fn test_trailing_elements_flushed() {
        use std::fs::File;
        // create a temporary directory structure
        let tmp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root_path = tmp_dir.path();

        dbg!(root_path);

        // create a couple of files
        let file1_path = root_path.join("file1.txt");
        File::create(&file1_path).unwrap();
        let sub_dir_path = root_path.join("small_folder");
        fs::create_dir(&sub_dir_path).unwrap();
        let file2_path = sub_dir_path.join("file2.txt");
        File::create(&file2_path).unwrap();

        // run the scanner
        let path_str = root_path.to_str().unwrap();
        let scan = Scan::new(path_str);

        while !scan.complete.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
        }

        let tree = scan.tree_mutex.lock().unwrap();

        // verify that the root node actually registered children
        let root_node = tree.get_elem(0);
        assert!(
            root_node.has_children(),
            "Root node has no children registered"
        );

        // Ensure the total count of added nodes matches expected values
        // Expecting: Root node (0), "file1.txt", "small_folder", "file2.txt" -> 4 elements
        assert_eq!(tree.len(), 4, "Tree elements missing. Tree: {:?}", *tree);
    }
}
