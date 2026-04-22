use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use chrono::Local;

pub struct SessionLogger {
    path: PathBuf,
    start: Instant,
}

impl SessionLogger {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            start: Instant::now(),
        }
    }

    pub fn log(&self, event: &str, detail: &str) {
        let elapsed = self.start.elapsed().as_millis();
        let timestamp = Local::now().format("%H:%M:%S%.3f");
        let line = format!("[{timestamp} +{elapsed:>6}ms] {event}: {detail}\n");
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }
}
