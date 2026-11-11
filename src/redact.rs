//! Keeps node URLs, and the keys hosted nodes put in them, out of everything the process
//! prints.
//!
//! reqwest quotes the full request URL in its errors and alloy opens a span with it on
//! every request, so any log line or error can carry a key. The secret parts of each node
//! URL are registered here once at startup and again on every reload, and every tracing
//! writer and every error that leaves the process passes through [`redact`].

use std::io::{self, Write};
use std::sync::RwLock;

use url::Url;

/// The env vars that hold node URLs.
const URL_VARS: [&str; 3] = ["RPC_URL", "WS_URL", "FORK_URL"];
/// What a secret fragment is replaced with.
const REDACTED: &str = "<redacted>";

/// Every secret fragment registered so far, longest first, so a URL is replaced whole
/// before any part of it is. Fragments are never removed: a URL dropped from the env can
/// still show up in an error from the chain built with it.
static SECRETS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// The fragments of `raw` that may hold a key: the URL as written, its normalized form
/// (which reqwest prints, and which can differ, for example by a trailing slash), its path
/// and query when the path is more than `/`, and the userinfo password.
fn fragments(raw: &str) -> Vec<String> {
  let raw = raw.trim();
  let mut found = vec![raw.to_string()];
  if let Ok(url) = Url::parse(raw) {
    found.push(url.to_string());
    if url.path() != "/" {
      let mut tail = url.path().to_string();
      if let Some(query) = url.query() {
        tail.push('?');
        tail.push_str(query);
      }
      found.push(tail);
    }
    if let Some(password) = url.password() {
      found.push(password.to_string());
    }
  }
  found.retain(|fragment| !fragment.is_empty());
  found
}

/// Registers the secret fragments of `raw`, a node URL.
pub fn register(raw: &str) {
  let mut secrets = SECRETS.write().unwrap_or_else(|e| e.into_inner());
  for fragment in fragments(raw) {
    if !secrets.contains(&fragment) {
      secrets.push(fragment);
    }
  }
  secrets.sort_by_key(|fragment| std::cmp::Reverse(fragment.len()));
}

/// Registers the node URLs the env holds now; unset vars are skipped. Called at startup
/// and after every reload rereads the env file.
pub fn register_env() {
  for var in URL_VARS {
    if let Ok(raw) = std::env::var(var) {
      register(&raw);
    }
  }
}

/// `text` with every registered fragment replaced by [`REDACTED`].
pub fn redact(text: &str) -> String {
  let secrets = SECRETS.read().unwrap_or_else(|e| e.into_inner());
  let mut clean = text.to_string();
  for fragment in secrets.iter() {
    if clean.contains(fragment.as_str()) {
      clean = clean.replace(fragment.as_str(), REDACTED);
    }
  }
  clean
}

/// Tracing writer that redacts each formatted line before passing it on to the writer it
/// wraps, the one choke point every log line goes through, third party ones included.
pub struct Redacting<W>(pub W);

impl<W: Write> Write for Redacting<W> {
  /// Redacts and writes one formatted event; tracing writes each event in a single call.
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    let clean = redact(&String::from_utf8_lossy(buf));
    self.0.write_all(clean.as_bytes())?;
    Ok(buf.len())
  }

  /// Flushes the wrapped writer.
  fn flush(&mut self) -> io::Result<()> {
    self.0.flush()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The key, the userinfo password and the whole URL all go.
  #[test]
  fn redacts_every_fragment() {
    register("wss://user:HUNTER2@redact.example/v1/PATHKEY?token=QUERYKEY");
    let text =
      redact("connect to wss://user:HUNTER2@redact.example/v1/PATHKEY?token=QUERYKEY failed");
    for secret in ["HUNTER2", "PATHKEY", "QUERYKEY"] {
      assert!(!text.contains(secret), "{text}");
    }
    let text = redact("auth with HUNTER2 then GET /v1/PATHKEY?token=QUERYKEY");
    for secret in ["HUNTER2", "PATHKEY", "QUERYKEY"] {
      assert!(!text.contains(secret), "{text}");
    }
  }

  /// A line quoting the URL in the normalized form reqwest prints is redacted by the
  /// writer, though the registered URL was written differently.
  #[test]
  fn writer_redacts_normalized_url() {
    register("HTTPS://Writer.Example:443/v3/WRITERKEY");
    let mut writer = Redacting(Vec::new());
    let line =
      "WARN request failed: error sending request for url (https://writer.example/v3/WRITERKEY)\n";
    writer.write_all(line.as_bytes()).unwrap();
    let written = String::from_utf8(writer.0).unwrap();
    assert!(!written.contains("WRITERKEY"), "{written}");
    assert!(!written.contains("writer.example"), "{written}");
    assert!(written.contains("request failed"), "{written}");
  }

  /// Text with nothing registered in it comes back unchanged.
  #[test]
  fn plain_text_unchanged() {
    assert_eq!(redact("block 123 applied"), "block 123 applied");
  }
}
