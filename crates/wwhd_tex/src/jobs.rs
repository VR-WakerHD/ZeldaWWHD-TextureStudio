//! Shared progress, cancellation, pause and log for long-running stages. The
//! GUI reads it every frame; workers update it from any thread.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Default)]
struct Inner {
    stage: String,
    current: String,
    started: Option<Instant>,
    log: Vec<String>,
    log_file: Option<std::path::PathBuf>,
}

#[derive(Clone, Default)]
pub struct Progress {
    inner: Arc<Mutex<Inner>>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

pub struct Snapshot {
    pub stage: String,
    pub current: String,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub elapsed_s: f32,
    pub running: bool,
    pub paused: bool,
}

impl Progress {
    pub fn with_log_file(path: std::path::PathBuf) -> Progress {
        let p = Progress::default();
        p.inner.lock().unwrap().log_file = Some(path);
        p
    }
    pub fn begin(&self, stage: &str, total: usize) {
        let mut i = self.inner.lock().unwrap();
        i.stage = stage.to_string();
        i.current.clear();
        i.started = Some(Instant::now());
        drop(i);
        self.done.store(0, Ordering::Relaxed);
        self.failed.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        self.log(format!("{stage}: {total} items"));
    }
    pub fn item(&self, name: &str) {
        self.inner.lock().unwrap().current = name.to_string();
    }
    pub fn step(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }
    pub fn steps(&self, n: usize) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn fail(&self, msg: String) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.log(format!("FAILED {msg}"));
    }
    pub fn log(&self, msg: String) {
        let line = format!("[{}] {msg}", timestamp());
        let mut i = self.inner.lock().unwrap();
        if let Some(f) = &i.log_file {
            use std::io::Write;
            if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
                let _ = writeln!(fh, "{line}");
            }
        }
        i.log.push(line);
        if i.log.len() > 5000 {
            i.log.drain(..1000);
        }
    }
    pub fn log_lines(&self, from: usize) -> (usize, Vec<String>) {
        let i = self.inner.lock().unwrap();
        let n = i.log.len();
        (n, i.log[from.min(n)..].to_vec())
    }
    pub fn snapshot(&self) -> Snapshot {
        let i = self.inner.lock().unwrap();
        Snapshot {
            stage: i.stage.clone(),
            current: i.current.clone(),
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            elapsed_s: i.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0),
            running: self.running.load(Ordering::Relaxed),
            paused: self.pause.load(Ordering::Relaxed),
        }
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause.store(false, Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }
    pub fn set_paused(&self, p: bool) {
        self.pause.store(p, Ordering::Relaxed);
    }
    /// Block while paused (checked between items).
    pub fn wait_if_paused(&self) {
        while self.pause.load(Ordering::Relaxed) && !self.cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    pub fn set_running(&self, r: bool) {
        if r {
            self.cancel.store(false, Ordering::Relaxed);
        }
        self.running.store(r, Ordering::Relaxed);
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

fn timestamp() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, sec) = ((s / 3600) % 24, (s / 60) % 60, s % 60);
    format!("{h:02}:{m:02}:{sec:02}Z")
}
