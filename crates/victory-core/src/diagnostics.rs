//! Opt-in JSONL for structured failures at selected emulator boundaries.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const FOOTER_BYTES: u64 = 1024;
const WIN32_CODES: &str =
    "https://learn.microsoft.com/windows/win32/debug/system-error-codes--0-499-";
static SESSION: OnceLock<Session> = OnceLock::new();

struct Session {
    id: String,
    state: Mutex<State>,
}

struct State {
    file: File,
    bytes: u64,
    sequence: u64,
    dropped: u64,
    failed: bool,
    finished: bool,
}

/// Open a new output file outside any VIctory source tree.
pub fn start(path: &Path) -> anyhow::Result<()> {
    crate::ensure_outside_repo(path)?;
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let session = Session::new(file);
    session.event(
        "session.started",
        json!({
            "level": "INFO",
            "victory_version": env!("CARGO_PKG_VERSION"),
            "git_commit": option_env!("VICTORY_BUILD_COMMIT").filter(|commit| {
                matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
            "platform": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "diagnostics_feature": cfg!(feature = "diagnostics"),
        }),
    );
    SESSION
        .set(session)
        .map_err(|_| anyhow::anyhow!("a diagnostic session is already active"))
}

/// Capture a monotonic start time only when structured diagnostics are active.
pub fn operation_started() -> Option<Instant> {
    SESSION.get().map(|_| Instant::now())
}

/// End the CLI session, including the number of events omitted at the file limit.
pub fn finish(success: bool) {
    if let Some(session) = SESSION.get() {
        session.finish(success);
    }
}

/// The selected boundaries allow only static operation names, trusted type
/// identifiers, and numeric status codes into the export.
#[derive(Clone, Copy)]
pub enum IoOperation {
    InspectImage,
    ReadImage,
    CreateImageDirectory,
    MapImage,
}

impl IoOperation {
    /// Record the failure while leaving the caller's original error intact.
    pub fn failed(self, error: &io::Error, started: Option<Instant>) {
        if let Some(session) = SESSION.get() {
            session.event("operation.failed", self.fields(error, started));
        }
    }

    fn fields(self, error: &io::Error, started: Option<Instant>) -> Value {
        let (component, module, function, operation, expected) = match self {
            Self::InspectImage => (
                "cli",
                "victory::inspect",
                "run",
                "read_image",
                "image_bytes",
            ),
            Self::ReadImage => ("cli", "victory::run", "run", "read_image", "image_bytes"),
            Self::CreateImageDirectory => (
                "loader",
                "victory_loader::windows",
                "load",
                "create_image_directory",
                "directory_available",
            ),
            Self::MapImage => (
                "loader",
                "victory_loader::windows",
                "map_image",
                "LoadLibraryExW",
                "module_handle",
            ),
        };
        let code = error.raw_os_error();
        let domain = if cfg!(windows) { "WIN32" } else { "OS" };
        let definition = if cfg!(windows) {
            match code {
                Some(2) => Some(("ERROR_FILE_NOT_FOUND", "The specified file was not found.")),
                Some(3) => Some(("ERROR_PATH_NOT_FOUND", "The specified path was not found.")),
                Some(5) => Some(("ERROR_ACCESS_DENIED", "Access was denied.")),
                Some(126) => Some(("ERROR_MOD_NOT_FOUND", "A required module was not found.")),
                _ => None,
            }
        } else {
            None
        };
        let (category, dependency, observed) = match self {
            Self::MapImage => (
                if code == Some(126) && cfg!(windows) {
                    "DEPENDENCY_UNAVAILABLE"
                } else {
                    "DEPENDENCY_ERROR"
                },
                json!({
                    "name": "Win32 loader",
                    "binding": "windows-sys",
                    "binding_locked_version": locked_version("windows-sys"),
                }),
                "null_handle",
            ),
            _ => ("IO_ERROR", json!({"name": "Rust std::fs"}), "io_error"),
        };
        json!({
            "level": "ERROR",
            "category": category,
            "component": component,
            "module": module,
            "function": function,
            "operation": operation,
            "expected": expected,
            "observed": observed,
            "dependency": dependency,
            "duration_us": started.map(|time| time.elapsed().as_micros()),
            "error": {
                "type": "std::io::Error",
                "kind": format!("{:?}", error.kind()),
                "native_code": code.map(|value| json!({
                    "domain": domain,
                    "value": value,
                    "symbol": definition.map(|(symbol, _)| symbol),
                })),
                "native_message_status": "omitted",
                "recoverable": null,
            },
            "diagnostic": {
                "definition_status": if definition.is_some() { "documented" } else { "undetermined" },
                "documented_meaning": definition.map(|(_, meaning)| meaning),
                "root_cause_status": "undetermined",
                "confirmed_facts": [observed],
                "hypotheses": [],
                "documentation": definition.map_or_else(Vec::new, |_| vec![WIN32_CODES]),
            },
        })
    }
}

fn locked_version(name: &str) -> Option<&'static str> {
    let lockfile = include_str!("../../../Cargo.lock");
    let mut versions =
        lockfile
            .lines()
            .zip(lockfile.lines().skip(1))
            .filter_map(|(package, version)| {
                let package = package.strip_prefix("name = \"")?.strip_suffix('"')?;
                if package != name {
                    return None;
                }
                version.strip_prefix("version = \"")?.strip_suffix('"')
            });
    let version = versions.next()?;
    versions.next().is_none().then_some(version)
}

impl Session {
    fn new(file: File) -> Self {
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos());
        Self {
            id: format!("{clock:x}-{:x}", std::process::id()),
            state: Mutex::new(State {
                file,
                bytes: 0,
                sequence: 0,
                dropped: 0,
                failed: false,
                finished: false,
            }),
        }
    }

    fn event(&self, kind: &str, fields: Value) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.failed && !state.finished {
            self.write(&mut state, kind, fields);
        }
    }

    fn finish(&self, success: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.failed && !state.finished {
            let dropped = state.dropped;
            self.write(
                &mut state,
                "session.finished",
                json!({"level": "INFO", "success": success, "dropped_events": dropped}),
            );
            state.finished = true;
        }
    }

    fn write(&self, state: &mut State, kind: &str, mut event: Value) {
        let sequence = state.sequence + 1;
        let event_id = format!("{}-{sequence}", self.id);
        let utc_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|time| time.as_millis());
        event["schema_version"] = json!("1.0");
        event["event"] = json!(kind);
        event["timestamp_unix_ms"] = json!(utc_millis);
        event["session_id"] = json!(&self.id);
        event["sequence"] = json!(sequence);
        if kind == "operation.failed" {
            event["correlation_id"] = json!(&event_id);
        }
        event["event_id"] = json!(event_id);

        let mut line = event.to_string();
        line.push('\n');
        let size = line.len() as u64;
        let limit = if kind == "session.finished" {
            MAX_FILE_BYTES
        } else {
            MAX_FILE_BYTES - FOOTER_BYTES
        };
        if state.bytes + size > limit {
            state.dropped += 1;
        } else if state.file.write_all(line.as_bytes()).is_err() {
            state.failed = true;
            eprintln!("VIctory: diagnostic output could not be written");
        } else {
            state.bytes += size;
            state.sequence = sequence;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_ordered_events_without_the_original_io_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("diagnostics.jsonl");
        let session = Session::new(File::create(&path).unwrap());
        session.event("session.started", json!({"level": "INFO"}));
        let error = io::Error::other("private path and guest output");
        session.event(
            "operation.failed",
            IoOperation::ReadImage.fields(&error, None),
        );
        session.finish(false);
        let output = std::fs::read_to_string(path).unwrap();
        assert!(!output.contains("private path"));
        let events: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["session_id"], events[2]["session_id"]);
        assert_eq!(events[1]["event_id"], events[1]["correlation_id"]);
        assert_eq!(events[1]["sequence"], 2);
        assert_eq!(events[1]["diagnostic"]["definition_status"], "undetermined");
        assert_eq!(events[2]["success"], false);
    }

    #[test]
    fn preserves_space_for_the_summary_at_the_limit() {
        let file = tempfile::tempfile().unwrap();
        let session = Session::new(file);
        session.state.lock().unwrap().bytes = MAX_FILE_BYTES - FOOTER_BYTES;
        session.event("operation.failed", json!({"level": "ERROR"}));
        session.finish(false);
        let state = session.state.lock().unwrap();
        assert_eq!(state.dropped, 1);
        assert_eq!(state.sequence, 1);
        assert!(state.finished && state.bytes <= MAX_FILE_BYTES);
    }

    #[cfg(windows)]
    #[test]
    fn explains_a_loader_code_without_guessing_the_root_cause() {
        let error = io::Error::from_raw_os_error(126);
        let report = IoOperation::MapImage.fields(&error, None);
        assert_eq!(report["error"]["native_code"]["value"], 126);
        assert_eq!(
            report["error"]["native_code"]["symbol"],
            "ERROR_MOD_NOT_FOUND"
        );
        assert_eq!(report["diagnostic"]["root_cause_status"], "undetermined");
    }
}
