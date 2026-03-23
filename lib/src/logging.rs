use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{self, MakeWriter, writer::EitherWriter},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

#[derive(Clone)]
struct StdoutGate {
    enabled: Arc<Mutex<bool>>,
}

impl StdoutGate {
    fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(Mutex::new(enabled)),
        }
    }

    fn get(&self) -> bool {
        *self.enabled.lock().expect("stdout gate lock poisoned")
    }

    fn set(&self, enabled: bool) {
        *self.enabled.lock().expect("stdout gate lock poisoned") = enabled;
    }
}

enum StdoutWriter {
    Enabled(io::Stdout),
    Disabled(io::Sink),
}

impl Write for StdoutWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Enabled(stdout) => stdout.write(buf),
            Self::Disabled(sink) => sink.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Enabled(stdout) => stdout.flush(),
            Self::Disabled(sink) => sink.flush(),
        }
    }
}

#[derive(Clone)]
struct StdoutMakeWriter {
    gate: StdoutGate,
}

impl<'a> MakeWriter<'a> for StdoutMakeWriter {
    type Writer = StdoutWriter;

    fn make_writer(&'a self) -> Self::Writer {
        if self.gate.get() {
            StdoutWriter::Enabled(io::stdout())
        } else {
            StdoutWriter::Disabled(io::sink())
        }
    }
}

#[derive(Clone)]
struct FileMakeWriter {
    file: Arc<Mutex<Option<Arc<Mutex<std::fs::File>>>>>,
}

impl FileMakeWriter {
    fn new() -> Self {
        Self {
            file: Arc::new(Mutex::new(None)),
        }
    }

    fn set_file(&self, file: Option<Arc<Mutex<std::fs::File>>>) {
        *self.file.lock().expect("file writer lock poisoned") = file;
    }
}

impl<'a> MakeWriter<'a> for FileMakeWriter {
    type Writer = EitherWriter<FileGuardWriter, io::Sink>;

    fn make_writer(&'a self) -> Self::Writer {
        let file = self.file.lock().expect("file writer lock poisoned").clone();
        match file {
            Some(file) => EitherWriter::A(FileGuardWriter { file }),
            None => EitherWriter::B(io::sink()),
        }
    }
}

struct FileGuardWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl Write for FileGuardWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.lock().expect("log file lock poisoned").write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.lock().expect("log file lock poisoned").flush()
    }
}

static LOGGER_STATE: OnceLock<Arc<LoggerState>> = OnceLock::new();

pub struct StdoutLoggingGuard {
    state: Arc<LoggerState>,
    previous_enabled: bool,
}

impl Drop for StdoutLoggingGuard {
    fn drop(&mut self) {
        self.state.set_stdout_enabled(self.previous_enabled);
    }
}

pub fn init_stdout_logging() {
    let state = LOGGER_STATE
        .get_or_init(|| Arc::new(LoggerState::new()))
        .clone();
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(StdoutMakeWriter {
                    gate: state.stdout_gate.clone(),
                })
                .with_ansi(false)
                .with_target(true),
        )
        .with(
            fmt::layer()
                .with_writer(state.file_writer.clone())
                .with_ansi(false)
                .with_target(true),
        )
        .with(LevelFilter::from_level(tracing::Level::INFO))
        .try_init();
}

pub async fn enable_workspace_file_logging(
    workspace_directory: impl AsRef<Path>,
) -> io::Result<PathBuf> {
    let state = LOGGER_STATE
        .get_or_init(|| Arc::new(LoggerState::new()))
        .clone();
    let multicode_dir = workspace_directory.as_ref().join(".multicode");
    tokio::fs::create_dir_all(&multicode_dir).await?;
    let log_path = multicode_dir.join("multicode.log");

    let file = tokio::task::spawn_blocking({
        let log_path = log_path.clone();
        move || {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map(|file| Arc::new(Mutex::new(file)))
        }
    })
    .await
    .map_err(join_error_to_io)??;

    state.set_file(Some(file));
    Ok(log_path)
}

/// While the TUI is running, stdout logging must be turned off.
pub fn suppress_stdout_logging() -> StdoutLoggingGuard {
    let state = LOGGER_STATE
        .get_or_init(|| Arc::new(LoggerState::new()))
        .clone();
    let previous_enabled = state.stdout_enabled();
    state.set_stdout_enabled(false);
    StdoutLoggingGuard {
        state,
        previous_enabled,
    }
}

pub fn stdout_writer() -> impl Write + Send {
    StdoutMakeWriter {
        gate: LOGGER_STATE
            .get_or_init(|| Arc::new(LoggerState::new()))
            .stdout_gate
            .clone(),
    }
    .make_writer()
}

fn join_error_to_io(err: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("failed to join logging task: {err}"))
}

struct LoggerState {
    stdout_gate: StdoutGate,
    file_writer: FileMakeWriter,
}

impl LoggerState {
    fn new() -> Self {
        Self {
            stdout_gate: StdoutGate::new(true),
            file_writer: FileMakeWriter::new(),
        }
    }

    fn stdout_enabled(&self) -> bool {
        self.stdout_gate.get()
    }

    fn set_stdout_enabled(&self, enabled: bool) {
        self.stdout_gate.set(enabled);
    }

    fn set_file(&self, file: Option<Arc<Mutex<std::fs::File>>>) {
        self.file_writer.set_file(file);
    }
}

pub fn log_file_enable_failed(path: &Path, error: &io::Error) {
    tracing::error!(
        log_path = %path.display(),
        error = %error,
        "failed to enable workspace file logging"
    );
}

pub fn log_file_enabled(path: &Path) {
    tracing::info!(log_path = %path.display(), "workspace file logging enabled");
}

pub fn log_startup(config_path: &Path) {
    tracing::info!(config_path = %config_path.display(), "multicode starting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "multicode-logging-{}-{}",
                std::process::id(),
                unique
            ));
            fs::create_dir_all(&path).expect("test dir should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn stdout_gate_toggles_and_restores() {
        let state = LoggerState::new();
        assert!(state.stdout_enabled());

        state.set_stdout_enabled(false);
        assert!(!state.stdout_enabled());

        state.set_stdout_enabled(true);
        assert!(state.stdout_enabled());
    }

    #[test]
    fn stdout_make_writer_switches_between_stdout_and_sink() {
        let state = LoggerState::new();
        let make_writer = StdoutMakeWriter {
            gate: state.stdout_gate.clone(),
        };

        assert!(matches!(
            make_writer.make_writer(),
            StdoutWriter::Enabled(_)
        ));
        state.set_stdout_enabled(false);
        assert!(matches!(
            make_writer.make_writer(),
            StdoutWriter::Disabled(_)
        ));
    }

    #[test]
    fn file_writer_switches_between_sink_and_file() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let state = LoggerState::new();
            let mut sink_writer = state.file_writer.make_writer();
            sink_writer
                .write_all(b"before")
                .expect("sink writer should accept writes");
            sink_writer.flush().expect("sink writer should flush");

            let root = TestDir::new();
            let multicode_dir = root.path().join(".multicode");
            tokio::fs::create_dir_all(&multicode_dir)
                .await
                .expect("multicode dir should be created");
            let log_path = multicode_dir.join("multicode.log");
            let file = Arc::new(Mutex::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .expect("log file should open"),
            ));

            state.set_file(Some(file));
            let mut file_writer = state.file_writer.make_writer();
            file_writer
                .write_all(b"hello log")
                .expect("file writer should write");
            file_writer.flush().expect("file writer should flush");

            let written = fs::read_to_string(&log_path).expect("log file should be readable");
            assert_eq!(written, "hello log");
        });
    }
}
