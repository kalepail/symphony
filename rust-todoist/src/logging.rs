use std::{
    env,
    ffi::OsString,
    fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_LOG_RELATIVE_PATH: &str = "log/symphony.log";
const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_MAX_FILES: usize = 5;
const DEFAULT_LOG_FILTER: &str = "info";
const SYMPHONY_LOG_TARGETS: &[&str] = &["symphony=info", "symphony_rust=info"];

pub fn init(logs_root: Option<&Path>) -> Result<PathBuf, String> {
    let logs_root = match logs_root {
        Some(path) => path.to_path_buf(),
        None => default_logs_root()?,
    };
    let log_file = default_log_file(&logs_root);
    if let Some(parent) = log_file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let filter = build_filter();
    let sink = RotatingFileSink::new(log_file.clone(), DEFAULT_MAX_BYTES, DEFAULT_MAX_FILES);
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_ansi(false)
        .with_writer(move || sink.writer())
        .try_init()
        .map_err(|error| error.to_string())?;

    Ok(log_file)
}

pub fn default_log_file(logs_root: &Path) -> PathBuf {
    logs_root.join(DEFAULT_LOG_RELATIVE_PATH)
}

fn default_logs_root() -> Result<PathBuf, String> {
    env::current_dir().map_err(|error| error.to_string())
}

fn build_filter() -> EnvFilter {
    let default_spec = build_filter_spec(None);
    match env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(build_filter_spec(Some(&value)))
            .unwrap_or_else(|_| EnvFilter::new(default_spec)),
        Err(_) => EnvFilter::new(default_spec),
    }
}

fn build_filter_spec(env_spec: Option<&str>) -> String {
    let base = env_spec
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_LOG_FILTER);
    let mut directives = vec![base.to_string()];
    for target in SYMPHONY_LOG_TARGETS {
        let target_name = target.split('=').next().unwrap_or(target);
        if !has_target_directive(base, target_name) {
            directives.push((*target).to_string());
        }
    }

    directives.join(",")
}

fn has_target_directive(spec: &str, target_name: &str) -> bool {
    spec.split(',').map(str::trim).any(|directive| {
        directive == target_name
            || directive.starts_with(&format!("{target_name}="))
            || directive.starts_with(&format!("{target_name}["))
    })
}

#[derive(Clone)]
struct RotatingFileSink {
    state: Arc<Mutex<RotatingFileState>>,
}

struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
}

struct RotatingFileWriter {
    state: Arc<Mutex<RotatingFileState>>,
}

impl RotatingFileSink {
    fn new(path: PathBuf, max_bytes: u64, max_files: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(RotatingFileState {
                path,
                max_bytes,
                max_files,
            })),
        }
    }

    fn writer(&self) -> RotatingFileWriter {
        RotatingFileWriter {
            state: Arc::clone(&self.state),
        }
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .expect("rotating log writer mutex should not be poisoned");
        state.append(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let state = self
            .state
            .lock()
            .expect("rotating log writer mutex should not be poisoned");
        state.flush()
    }
}

impl RotatingFileState {
    fn append(&mut self, buf: &[u8]) -> io::Result<()> {
        self.rotate_if_needed(buf.len() as u64)?;
        let mut file = self.open_append_file()?;
        file.write_all(buf)?;
        file.flush()
    }

    fn flush(&self) -> io::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        self.open_append_file()?.flush()
    }

    fn open_append_file(&self) -> io::Result<fs::File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }

    fn rotate_if_needed(&self, incoming_bytes: u64) -> io::Result<()> {
        if self.max_bytes == 0 {
            return self.rotate_files();
        }

        let current_size = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };

        if current_size == 0 || current_size.saturating_add(incoming_bytes) <= self.max_bytes {
            Ok(())
        } else {
            self.rotate_files()
        }
    }

    fn rotate_files(&self) -> io::Result<()> {
        if self.max_files == 0 {
            return remove_if_exists(&self.path);
        }

        remove_if_exists(&self.rotated_path(self.max_files))?;
        for index in (1..self.max_files).rev() {
            let source = self.rotated_path(index);
            if source.exists() {
                let target = self.rotated_path(index + 1);
                remove_if_exists(&target)?;
                fs::rename(source, target)?;
            }
        }

        if self.path.exists() {
            let first = self.rotated_path(1);
            remove_if_exists(&first)?;
            fs::rename(&self.path, first)?;
        }

        Ok(())
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        let mut path = OsString::from(self.path.as_os_str());
        path.push(format!(".{index}"));
        PathBuf::from(path)
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path};

    use tempfile::tempdir;

    use super::{RotatingFileSink, build_filter_spec, default_log_file};

    #[test]
    fn default_log_file_uses_log_subdirectory() {
        assert_eq!(
            default_log_file(Path::new("/tmp/symphony")),
            Path::new("/tmp/symphony/log/symphony.log")
        );
    }

    #[test]
    fn rotating_writer_keeps_recent_history() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("log/symphony.log");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");

        let sink = RotatingFileSink::new(path.clone(), 6, 2);
        let mut writer = sink.writer();

        writer.write_all(b"aaaa\n").expect("write first");
        writer.write_all(b"bbbb\n").expect("write second");
        writer.write_all(b"cccc\n").expect("write third");
        writer.write_all(b"dddd\n").expect("write fourth");

        assert_eq!(std::fs::read_to_string(&path).expect("current"), "dddd\n");
        assert_eq!(
            std::fs::read_to_string(path.with_extension("log.1")).expect("first rotation"),
            "cccc\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_extension("log.2")).expect("second rotation"),
            "bbbb\n"
        );
        assert!(!path.with_extension("log.3").exists());
    }

    #[test]
    fn rotating_writer_can_discard_history_when_retention_is_zero() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("log/symphony.log");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");

        let sink = RotatingFileSink::new(path.clone(), 4, 0);
        let mut writer = sink.writer();

        writer.write_all(b"aaaa").expect("write first");
        writer.write_all(b"bbbb").expect("write second");

        assert_eq!(std::fs::read_to_string(&path).expect("current"), "bbbb");
        assert!(!path.with_extension("log.1").exists());
    }

    #[test]
    fn build_filter_spec_keeps_symphony_targets_visible_by_default() {
        assert_eq!(
            build_filter_spec(None),
            "info,symphony=info,symphony_rust=info"
        );
        assert_eq!(
            build_filter_spec(Some("warn")),
            "warn,symphony=info,symphony_rust=info"
        );
    }

    #[test]
    fn build_filter_spec_respects_explicit_symphony_directives() {
        assert_eq!(
            build_filter_spec(Some("warn,symphony=trace")),
            "warn,symphony=trace,symphony_rust=info"
        );
        assert_eq!(
            build_filter_spec(Some("warn,symphony=trace,symphony_rust=debug")),
            "warn,symphony=trace,symphony_rust=debug"
        );
    }
}
