//! Local structured-log and diagnostics ownership.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex},
};

use serde_json::Value;

use crate::{DiagnosticsSnapshot, Metrics, ObservabilityError, Redactor};

const MAX_LOG_LINE_BYTES: usize = 64 * 1024;
const MIN_LOG_SEGMENT_BYTES: u64 = 64 * 1024;
const MAX_LOG_SEGMENT_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogRotationPolicy {
    maximum_segment_bytes: u64,
    maximum_segments: usize,
}

impl LogRotationPolicy {
    pub fn new(
        maximum_segment_bytes: u64,
        maximum_segments: usize,
    ) -> Result<Self, ObservabilityError> {
        if !(MIN_LOG_SEGMENT_BYTES..=MAX_LOG_SEGMENT_BYTES).contains(&maximum_segment_bytes)
            || maximum_segments != 2
        {
            return Err(ObservabilityError::InvalidRotationPolicy);
        }
        Ok(Self {
            maximum_segment_bytes,
            maximum_segments,
        })
    }

    pub const fn maximum_segment_bytes(self) -> u64 {
        self.maximum_segment_bytes
    }

    pub const fn maximum_segments(self) -> usize {
        self.maximum_segments
    }
}

#[derive(Clone, Default)]
pub struct ObservabilityHandle {
    metrics: Metrics,
}

impl ObservabilityHandle {
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn snapshot(
        &self,
        captured_at_unix_nanos: i64,
    ) -> Result<DiagnosticsSnapshot, ObservabilityError> {
        self.metrics.snapshot(captured_at_unix_nanos)
    }
}

#[derive(Clone)]
pub struct LocalJsonLog {
    state: Arc<Mutex<LogState>>,
    redactor: Redactor,
}

enum LogState {
    Single(File),
    Rotating(RotatingLog),
}

struct RotatingLog {
    current: File,
    previous: File,
    current_bytes: u64,
    policy: LogRotationPolicy,
}

impl LocalJsonLog {
    pub fn from_file(file: File) -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState::Single(file))),
            redactor: Redactor,
        }
    }

    pub fn from_rotating_files(
        mut current: File,
        mut previous: File,
        policy: LogRotationPolicy,
    ) -> Result<Self, ObservabilityError> {
        let current_metadata = validate_private_file(&current)?;
        let previous_metadata = validate_private_file(&previous)?;
        if current_metadata.dev() == previous_metadata.dev()
            && current_metadata.ino() == previous_metadata.ino()
        {
            return Err(ObservabilityError::UnsafeLogFile);
        }
        if current_metadata.len() > policy.maximum_segment_bytes
            || previous_metadata.len() > policy.maximum_segment_bytes
        {
            return Err(ObservabilityError::UnsafeLogFile);
        }
        let current_bytes = recover_current_jsonl(&mut current, policy.maximum_segment_bytes)?;
        validate_complete_jsonl(&mut previous, policy.maximum_segment_bytes)?;
        current.seek(SeekFrom::End(0))?;
        Ok(Self {
            state: Arc::new(Mutex::new(LogState::Rotating(RotatingLog {
                current,
                previous,
                current_bytes,
                policy,
            }))),
            redactor: Redactor,
        })
    }

    pub fn write_event(&self, event: &Value) -> Result<(), ObservabilityError> {
        let sanitized = self.redactor.structured_event(event);
        let encoded = serde_json::to_vec(&sanitized)?;
        let line_bytes = encoded
            .len()
            .checked_add(1)
            .ok_or(ObservabilityError::LogSizeOverflow)?;
        if line_bytes > MAX_LOG_LINE_BYTES {
            return Err(ObservabilityError::LogLineTooLarge);
        }
        self.state
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?
            .write_line(&encoded)
    }

    pub fn sync(&self) -> Result<(), ObservabilityError> {
        self.state
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?
            .sync()
    }

    pub fn shutdown(self) -> Result<(), ObservabilityError> {
        self.sync()
    }
}

impl LogState {
    fn write_line(&mut self, encoded: &[u8]) -> Result<(), ObservabilityError> {
        match self {
            Self::Single(file) => {
                file.write_all(encoded)?;
                file.write_all(b"\n")?;
                Ok(())
            }
            Self::Rotating(log) => log.write_line(encoded),
        }
    }

    fn sync(&self) -> Result<(), ObservabilityError> {
        match self {
            Self::Single(file) => file.sync_data()?,
            Self::Rotating(log) => {
                log.current.sync_data()?;
                log.previous.sync_data()?;
            }
        }
        Ok(())
    }
}

impl RotatingLog {
    fn write_line(&mut self, encoded: &[u8]) -> Result<(), ObservabilityError> {
        let line_bytes = u64::try_from(encoded.len())
            .ok()
            .and_then(|length| length.checked_add(1))
            .ok_or(ObservabilityError::LogSizeOverflow)?;
        if line_bytes > self.policy.maximum_segment_bytes {
            return Err(ObservabilityError::LogLineTooLarge);
        }
        let next_size = self
            .current_bytes
            .checked_add(line_bytes)
            .ok_or(ObservabilityError::LogSizeOverflow)?;
        if self.current_bytes > 0 && next_size > self.policy.maximum_segment_bytes {
            self.rotate()?;
        }
        self.current.write_all(encoded)?;
        self.current.write_all(b"\n")?;
        self.current_bytes = self
            .current_bytes
            .checked_add(line_bytes)
            .ok_or(ObservabilityError::LogSizeOverflow)?;
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), ObservabilityError> {
        self.current.sync_data()?;
        self.current.seek(SeekFrom::Start(0))?;
        self.previous.set_len(0)?;
        self.previous.seek(SeekFrom::Start(0))?;
        let copied = std::io::copy(
            &mut Read::by_ref(&mut self.current).take(self.policy.maximum_segment_bytes + 1),
            &mut self.previous,
        )?;
        if copied != self.current_bytes {
            return Err(ObservabilityError::UnsafeLogFile);
        }
        self.previous.sync_data()?;
        self.current.set_len(0)?;
        self.current.seek(SeekFrom::Start(0))?;
        self.current_bytes = 0;
        Ok(())
    }
}

fn recover_current_jsonl(file: &mut File, maximum_bytes: u64) -> Result<u64, ObservabilityError> {
    let mut bytes = read_segment(file, maximum_bytes)?;
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        let recovered_length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        bytes.truncate(recovered_length);
        file.set_len(
            u64::try_from(recovered_length).map_err(|_| ObservabilityError::LogSizeOverflow)?,
        )?;
        file.sync_data()?;
    }
    validate_jsonl_bytes(&bytes)?;
    u64::try_from(bytes.len()).map_err(|_| ObservabilityError::LogSizeOverflow)
}

fn validate_complete_jsonl(file: &mut File, maximum_bytes: u64) -> Result<(), ObservabilityError> {
    let bytes = read_segment(file, maximum_bytes)?;
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return Err(ObservabilityError::UnsafeLogFile);
    }
    validate_jsonl_bytes(&bytes)
}

fn read_segment(file: &mut File, maximum_bytes: u64) -> Result<Vec<u8>, ObservabilityError> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take(maximum_bytes + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| ObservabilityError::LogSizeOverflow)? > maximum_bytes
    {
        return Err(ObservabilityError::UnsafeLogFile);
    }
    Ok(bytes)
}

fn validate_jsonl_bytes(bytes: &[u8]) -> Result<(), ObservabilityError> {
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let event =
            serde_json::from_slice::<Value>(line).map_err(|_| ObservabilityError::UnsafeLogFile)?;
        if !event.is_object() {
            return Err(ObservabilityError::UnsafeLogFile);
        }
    }
    Ok(())
}

fn validate_private_file(file: &File) -> Result<std::fs::Metadata, ObservabilityError> {
    let metadata = file.metadata()?;
    let safe = metadata.file_type().is_file()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1;
    if safe {
        Ok(metadata)
    } else {
        Err(ObservabilityError::UnsafeLogFile)
    }
}
