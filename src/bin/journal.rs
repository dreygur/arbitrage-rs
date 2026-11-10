//! Prints the last entries of the trade journal as a plain table.

use arbitrage::config::NetworkConfig;
use arbitrage::journal::read_last;
use arbitrage::types::Id;

/// Number of entries to print.
const ENTRIES: usize = 20;

/// Shortens a long hex string to its first and last characters.
fn short(text: &str) -> String {
  if text.len() <= 12 {
    return text.to_string();
  }
  format!("{}..{}", &text[..6], &text[text.len() - 4..])
}

/// Renders a base and token pair, or a dash when absent.
fn pair(pair: &Option<(Id, Id)>) -> String {
  match pair {
    Some((base, token)) => format!("{}/{}", short(&base.to_string()), short(&token.to_string())),
    None => "-".into(),
  }
}

/// Renders an optional value, or a dash when absent.
fn show<T: ToString>(value: Option<T>) -> String {
  value.map_or_else(|| "-".into(), |v| v.to_string())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
  dotenvy::dotenv().ok();
  let network = std::env::var("NETWORK").unwrap_or_else(|_| "ethereum".into());
  let config = NetworkConfig::load(&network)?;
  let (entries, skipped) = read_last(&config.journal, ENTRIES).await?;
  if skipped > 0 {
    println!("skipped {skipped} malformed lines");
  }
  println!(
    "{:<20} {:<7} {:<9} {:<27} {:<14} {:<14} {:<24} gas used",
    "time", "kind", "state", "pair", "account", "tx", "profit"
  );
  for entry in entries {
    let event = &entry.event;
    println!(
      "{:<20} {:<7} {:<9} {:<27} {:<14} {:<14} {:<24} {}",
      entry.ts,
      format!("{:?}", event.kind),
      format!("{:?}", event.state),
      pair(&event.detail.pair),
      short(&event.account),
      event.tx.as_deref().map_or_else(|| "-".into(), short),
      show(event.detail.actual_profit.or(event.detail.quoted_profit)),
      show(event.detail.gas_used),
    );
  }
  Ok(())
}
