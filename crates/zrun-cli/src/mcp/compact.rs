//! Loss-limited presentation of captured output; the capture itself stays byte-exact.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::execution::{Delivery, ResultRecord};

const TEXT_BYTES: usize = 1536;
const MAX_RUNS: usize = 24;
const BINARY_PREFIX: usize = 48;
const MAX_RECORDS: usize = 64;
pub(super) const PAGE_SIZE: usize = 4096;
const TTL: Duration = Duration::from_secs(300);

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// Callers pass at most PAGE_SIZE bytes. A base64 page can start inside a UTF-8 scalar.
fn base64(bytes: &[u8]) -> String {
    let size = z64::encoded_len(bytes.len(), z64::Mode::Standard).expect("bounded base64 size");
    let mut out = vec![0; size];
    z64::encode(bytes, &mut out, z64::Mode::Standard).expect("exact base64 capacity");
    String::from_utf8(out).expect("base64 is ASCII")
}

pub(crate) fn project(bytes: &[u8]) -> Value {
    project_with_digest(bytes, &sha256(bytes))
}

fn project_with_digest(bytes: &[u8], digest: &str) -> Value {
    let mut value = json!({"captured_bytes":bytes.len(), "sha256":digest});
    let Ok(text) = std::str::from_utf8(bytes) else {
        return binary_preview(bytes, value);
    };
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return binary_preview(bytes, value);
    }

    let mut lines = text.split_inclusive('\n').peekable();
    let mut runs = Vec::new();
    let mut remaining = TEXT_BYTES;
    let mut line_count: usize = 0;
    let mut clipped = false;
    while let Some(line) = lines.next() {
        let mut count = 1;
        while lines.next_if_eq(&line).is_some() {
            count += 1;
        }
        line_count += count;
        if runs.len() == MAX_RUNS || remaining == 0 {
            clipped = true;
            continue;
        }
        let length = line.floor_char_boundary(remaining.min(line.len()));
        let mut run = json!({"text":&line[..length], "count":count});
        if length < line.len() {
            run["clipped"] = true.into();
            clipped = true;
            remaining = 0;
        } else {
            remaining -= length;
        }
        runs.push(run);
    }
    value["encoding"] = "utf8".into();
    value["lines"] = line_count.into();
    value["runs"] = runs.into();
    value["preview_truncated"] = clipped.into();
    value
}

fn binary_preview(bytes: &[u8], mut value: Value) -> Value {
    let length = BINARY_PREFIX.min(bytes.len());
    value["encoding"] = "base64".into();
    value["preview"] = base64(&bytes[..length]).into();
    value["preview_truncated"] = (length != bytes.len()).into();
    value
}

// The handle is a bearer capability distinct from the public digest of the bytes.
// Allocate it before submission so an entropy failure cannot obscure a command's outcome.
pub(super) fn new_id() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    output_id: String,
    stream: Stream,
    #[serde(default)]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    PAGE_SIZE
}

impl Page {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        if self.output_id.len() != 32
            || !self
                .output_id
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err("expected a 32-character lowercase hexadecimal output_id");
        }
        if !(1..=PAGE_SIZE).contains(&self.limit) {
            return Err("limit must be between 1 and 4096 bytes");
        }
        Ok(())
    }
}

struct Record {
    id: String,
    since: Instant,
    result: Arc<ResultRecord>,
    stdout_sha256: String,
    stderr_sha256: String,
}

// One store per authorized listener (or a single local stdio client). No list operation,
// disk persistence, or cross-listener lookups. Each capture is already limited by Broker.
#[derive(Default)]
pub(super) struct Outputs(Mutex<VecDeque<Record>>);

impl Outputs {
    pub(super) fn compact(&self, id: String, delivery: &Delivery) -> Value {
        let result = &delivery.result;
        let stdout_sha256 = sha256(&result.stdout);
        let stderr_sha256 = sha256(&result.stderr);
        let stdout = project_with_digest(&result.stdout, &stdout_sha256);
        let stderr = project_with_digest(&result.stderr, &stderr_sha256);
        let mut entries = self.0.lock().expect("output store poisoned");
        let since = Instant::now();
        expire(&mut entries, since);
        if entries.len() == MAX_RECORDS {
            entries.pop_front();
        }
        entries.push_back(Record {
            id: id.clone(),
            since,
            result: result.clone(),
            stdout_sha256,
            stderr_sha256,
        });
        drop(entries);
        json!({"output_id": id, "disposition":delivery.disposition, "exit_code":result.exit_code,
               "error":result.error, "truncated":result.truncated, "stdout":stdout, "stderr":stderr})
    }

    pub(super) fn read(&self, page: &Page) -> Result<Value, &'static str> {
        let mut entries = self.0.lock().expect("output store poisoned");
        expire(&mut entries, Instant::now());
        let entry = entries
            .iter()
            .find(|e| e.id == page.output_id)
            .ok_or("output is unknown, expired or evicted on this endpoint")?;
        let (bytes, sha) = match page.stream {
            Stream::Stdout => (&entry.result.stdout, &entry.stdout_sha256),
            Stream::Stderr => (&entry.result.stderr, &entry.stderr_sha256),
        };
        if page.cursor > bytes.len() {
            return Err("cursor exceeds captured stream length");
        }
        let end = (page.cursor + page.limit).min(bytes.len());
        Ok(
            json!({"encoding":"base64", "data":base64(&bytes[page.cursor..end]),
                  "cursor":page.cursor, "next_cursor":end, "eof":end == bytes.len(),
                  "captured_bytes":bytes.len(), "sha256":sha, "truncated":entry.result.truncated}),
        )
    }
}

fn expire(entries: &mut VecDeque<Record>, now: Instant) {
    while entries
        .front()
        .is_some_and(|entry| now.duration_since(entry.since) >= TTL)
    {
        entries.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_folds_only_identical_consecutive_lines_including_the_separator() {
        let bytes = b"a\r\na\r\na\na\nb\na\na";
        let value = project(bytes);
        assert_eq!(
            value["runs"],
            json!([
                {"text":"a\r\n","count":2},{"text":"a\n","count":2},{"text":"b\n","count":1},
                {"text":"a\n","count":1},{"text":"a","count":1}
            ])
        );
        assert_eq!(value["lines"], 7);
        assert_eq!(value["preview_truncated"], false);
        assert_eq!(project(b"")["runs"], json!([]));
    }

    #[test]
    fn compact_preview_bounds_lines_groups_unicode_and_binary() {
        let big_line = format!("a{}\n", "界".repeat(TEXT_BYTES));
        let value = project(big_line.repeat(3).as_bytes());
        let run = &value["runs"][0];
        assert_eq!(run["count"], 3);
        assert_eq!(run["clipped"], true);
        assert!(run["text"].as_str().unwrap().len() <= TEXT_BYTES);
        assert_eq!(value["lines"], 3);
        assert_eq!(value["preview_truncated"], true);

        let text = (0..200).map(|n| format!("{n}\n")).collect::<String>();
        let value = project(text.as_bytes());
        assert_eq!(value["lines"], 200);
        assert_eq!(value["runs"].as_array().unwrap().len(), MAX_RUNS);
        assert_eq!(value["preview_truncated"], true);
        for bytes in [&[0xff; 100][..], &[0x1b; 100][..], &[0; 100][..]] {
            let value = project(bytes);
            assert_eq!(value["encoding"], "base64");
            assert_eq!(value["preview"], base64(&bytes[..BINARY_PREFIX]));
            assert_eq!(value["preview_truncated"], true);
        }
    }

    #[test]
    fn compact_handles_are_isolated_expire_and_evict_under_pressure() {
        let outputs = Outputs::default();
        let result = Delivery::rejected(io::Error::other("fixture"));
        let id = new_id().unwrap();
        let page = Page {
            output_id: id.clone(),
            stream: Stream::Stdout,
            cursor: 0,
            limit: 1,
        };
        outputs.compact(id.clone(), &result);
        assert!(outputs.read(&page).is_ok());
        assert!(Outputs::default().read(&page).is_err());
        for _ in 0..MAX_RECORDS {
            outputs.compact(new_id().unwrap(), &result);
        }
        assert!(outputs.read(&page).is_err());
        outputs.compact(id, &result);
        let mut entries = outputs.0.lock().unwrap();
        let past_deadline = entries.back().unwrap().since + TTL;
        expire(&mut entries, past_deadline);
        assert!(entries.is_empty());
    }
}
