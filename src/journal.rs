//! Append only JSON Lines journal of every transaction state change.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::error::{Error, Result};
use crate::types::TradeEvent;

/// Capacity of the channel between the engine and the writer task.
const CHANNEL_CAPACITY: usize = 1024;

/// One journal line: a trade event stamped with the wall clock time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
  /// UTC time the entry was recorded, RFC 3339 with seconds precision.
  pub ts: String,
  /// The state change being recorded.
  pub event: TradeEvent,
}

/// Formats Unix seconds as an RFC 3339 UTC timestamp.
///
/// Uses the days to civil date conversion so no time crate is needed.
fn format_rfc3339(secs: u64) -> String {
  let days = (secs / 86_400) as i64;
  let rem = secs % 86_400;
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let day = doy - (153 * mp + 2) / 5 + 1;
  let month = if mp < 10 { mp + 3 } else { mp - 9 };
  let year = yoe + era * 400 + i64::from(month <= 2);
  format!(
    "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
    rem / 3_600,
    rem % 3_600 / 60,
    rem % 60
  )
}

/// Returns the current time as an RFC 3339 UTC timestamp.
fn now_rfc3339() -> String {
  let secs = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
  format_rfc3339(secs)
}

/// Reports whether the file is non empty and its last byte is not a newline.
async fn ends_torn(path: &Path) -> Result<bool> {
  let len = tokio::fs::metadata(path).await?.len();
  if len == 0 {
    return Ok(false);
  }
  let mut file = tokio::fs::File::open(path).await?;
  file.seek(std::io::SeekFrom::Start(len - 1)).await?;
  let mut last = [0u8; 1];
  file.read_exact(&mut last).await?;
  Ok(last[0] != b'\n')
}

/// Serializes the entry and appends it as one flushed line, led by a
/// newline when `terminate` is set so a torn line is not glued to it.
async fn write_entry(
  file: &mut tokio::fs::File,
  entry: &JournalEntry,
  terminate: bool,
) -> Result<()> {
  let mut line = Vec::new();
  if terminate {
    line.push(b'\n');
  }
  serde_json::to_writer(&mut line, entry)?;
  line.push(b'\n');
  file.write_all(&line).await?;
  file.flush().await?;
  Ok(())
}

/// Handle for sending events to the journal writer task.
#[derive(Clone, Debug)]
pub struct JournalTx {
  /// Bounded channel into the writer task.
  tx: mpsc::Sender<JournalEntry>,
}

impl JournalTx {
  /// Stamps the event and queues it, waiting while the channel is full so
  /// that no event is dropped.
  pub async fn record(&self, event: TradeEvent) {
    let entry = JournalEntry {
      ts: now_rfc3339(),
      event,
    };
    if self.tx.send(entry).await.is_err() {
      error!("journal writer is gone, entry lost");
    }
  }
}

/// Owner of the journal file and its writer task.
pub struct Journal;

impl Journal {
  /// Creates the parent directories, opens the file for appending and
  /// starts the writer task. Write errors are logged and the task goes on.
  pub async fn spawn(path: impl Into<PathBuf>) -> Result<JournalTx> {
    let path = path.into();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
      tokio::fs::create_dir_all(dir).await?;
    }
    let torn = path.exists() && ends_torn(&path).await?;
    let mut file = tokio::fs::OpenOptions::new()
      .append(true)
      .create(true)
      .open(&path)
      .await?;
    let (tx, mut rx) = mpsc::channel::<JournalEntry>(CHANNEL_CAPACITY);
    tokio::spawn(async move {
      let mut terminate = torn;
      while let Some(entry) = rx.recv().await {
        match write_entry(&mut file, &entry, terminate).await {
          Ok(()) => terminate = false,
          Err(e) => {
            terminate = true;
            error!(error = %e, "journal write failed");
          }
        }
      }
    });
    Ok(JournalTx { tx })
  }
}

/// Reads the journal and returns its last `n` entries in file order.
///
/// A malformed line is an error naming its line number, except a final line
/// without a trailing newline, which a crash may have torn and is skipped.
// Reads the whole file: a journal holds a handful of lines per trade, so it stays small.
pub async fn read_last(path: &Path, n: usize) -> Result<Vec<JournalEntry>> {
  let text = tokio::fs::read_to_string(path).await?;
  let unterminated = !text.ends_with('\n');
  let total = text.lines().count();
  let mut entries = Vec::new();
  for (index, line) in text.lines().enumerate() {
    if line.trim().is_empty() {
      continue;
    }
    match serde_json::from_str(line) {
      Ok(entry) => entries.push(entry),
      Err(e) if unterminated && index + 1 == total => {
        warn!(line = index + 1, error = %e, "skipping torn final journal line");
      }
      Err(e) => {
        return Err(Error::Config(format!("journal line {}: {e}", index + 1)));
      }
    }
  }
  let start = entries.len().saturating_sub(n);
  Ok(entries.split_off(start))
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;
  use crate::types::{Amount, EntryKind, TradeDetail, TradeState};

  /// Builds an event with the given ledger id and a large amount.
  fn event(id: u64) -> TradeEvent {
    TradeEvent {
      id,
      kind: EntryKind::Trade,
      state: TradeState::Executed,
      block: 100 + id,
      account: "0xabc".into(),
      nonce: id,
      tx: Some("0xdead".into()),
      detail: TradeDetail {
        amount: Some(Amount::from(10u64).pow(Amount::from(30u64))),
        ..Default::default()
      },
    }
  }

  /// Returns a fresh path inside a unique temp directory.
  fn temp_path(name: &str, file: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("journal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join(file)
  }

  /// Waits until the file holds `count` lines, since the writer is async.
  async fn wait_for_lines(path: &Path, count: usize) {
    for _ in 0..200 {
      if let Ok(text) = tokio::fs::read_to_string(path).await
        && text.lines().count() >= count
      {
        return;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("journal never reached {count} lines");
  }

  /// The epoch and a leap day format as RFC 3339.
  #[test]
  fn formats_rfc3339() {
    assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(format_rfc3339(1_794_137_045), "2026-11-08T11:24:05Z");
  }

  /// An entry is one JSON line and amounts are decimal strings.
  #[test]
  fn entry_roundtrips_as_json_line() {
    let entry = JournalEntry {
      ts: "2026-11-08T11:24:05Z".into(),
      event: event(1),
    };
    let line = serde_json::to_string(&entry).unwrap();
    assert!(!line.contains('\n'));
    assert!(line.contains("\"1000000000000000000000000000000\""));
    assert_eq!(serde_json::from_str::<JournalEntry>(&line).unwrap(), entry);
  }

  /// Reopening the journal appends after the existing lines.
  #[tokio::test]
  async fn appends_across_reopen() {
    let path = temp_path("reopen", "trades.jsonl");
    let tx = Journal::spawn(&path).await.unwrap();
    tx.record(event(1)).await;
    tx.record(event(2)).await;
    wait_for_lines(&path, 2).await;
    drop(tx);
    let tx = Journal::spawn(&path).await.unwrap();
    tx.record(event(3)).await;
    wait_for_lines(&path, 3).await;
    let ids: Vec<u64> = read_last(&path, 10)
      .await
      .unwrap()
      .iter()
      .map(|e| e.event.id)
      .collect();
    assert_eq!(ids, vec![1, 2, 3]);
  }

  /// Missing parent directories are created.
  #[tokio::test]
  async fn creates_missing_dir() {
    let path = temp_path("mkdir", "a/b/trades.jsonl");
    let tx = Journal::spawn(&path).await.unwrap();
    tx.record(event(1)).await;
    wait_for_lines(&path, 1).await;
    assert_eq!(read_last(&path, 5).await.unwrap().len(), 1);
  }

  /// The tail comes back in file order and a bad line names its number.
  #[tokio::test]
  async fn read_last_returns_tail_in_order() {
    let path = temp_path("tail", "trades.jsonl");
    let tx = Journal::spawn(&path).await.unwrap();
    for id in 1..=5 {
      tx.record(event(id)).await;
    }
    wait_for_lines(&path, 5).await;
    let ids: Vec<u64> = read_last(&path, 3)
      .await
      .unwrap()
      .iter()
      .map(|e| e.event.id)
      .collect();
    assert_eq!(ids, vec![3, 4, 5]);
    let mut text = tokio::fs::read_to_string(&path).await.unwrap();
    text.push_str("not json\n");
    tokio::fs::write(&path, text).await.unwrap();
    let err = read_last(&path, 3).await.unwrap_err();
    assert!(err.to_string().contains("line 6"));
  }

  /// A torn last line gets a newline on spawn so the next entry stands alone.
  #[tokio::test]
  async fn spawn_terminates_torn_last_line() {
    let path = temp_path("torn-spawn", "trades.jsonl");
    tokio::fs::create_dir_all(path.parent().unwrap())
      .await
      .unwrap();
    tokio::fs::write(&path, "{\"ts\":\"2026-11-08T00:00:00Z\",\"eve")
      .await
      .unwrap();
    let tx = Journal::spawn(&path).await.unwrap();
    tx.record(event(7)).await;
    wait_for_lines(&path, 2).await;
    let text = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("\"eve"));
    assert_eq!(
      serde_json::from_str::<JournalEntry>(lines[1])
        .unwrap()
        .event
        .id,
      7
    );
  }

  /// A malformed final line without a newline is skipped, interior ones error.
  #[tokio::test]
  async fn read_last_skips_torn_final_line() {
    let path = temp_path("torn-read", "trades.jsonl");
    let tx = Journal::spawn(&path).await.unwrap();
    tx.record(event(1)).await;
    wait_for_lines(&path, 1).await;
    let mut text = tokio::fs::read_to_string(&path).await.unwrap();
    text.push_str("{\"ts\":\"torn");
    tokio::fs::write(&path, &text).await.unwrap();
    let entries = read_last(&path, 5).await.unwrap();
    assert_eq!(entries.len(), 1);
    text.push('\n');
    tokio::fs::write(&path, &text).await.unwrap();
    let err = read_last(&path, 5).await.unwrap_err();
    assert!(err.to_string().contains("line 2"));
  }
}
