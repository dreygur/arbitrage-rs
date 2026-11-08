//! Signers for the configured accounts, read from mnemonic and private key env vars.

use std::collections::HashSet;

use alloy::signers::local::coins_bip39::English;
use alloy::signers::local::{MnemonicBuilder, PrivateKeySigner};

use crate::config::AccountSource;
use crate::error::{Error, Result};

/// Reads the env var `name`, treating an unset or empty value as missing.
fn secret(name: &str) -> Result<String> {
  std::env::var(name)
    .ok()
    .filter(|value| !value.trim().is_empty())
    .ok_or_else(|| Error::Env(name.to_string()))
}

/// The signers one source yields, in its index order.
fn source_signers(source: &AccountSource) -> Result<Vec<PrivateKeySigner>> {
  match source {
    AccountSource::Mnemonic { env, indices } => {
      let phrase = secret(env)?;
      indices
        .iter()
        .map(|&index| {
          MnemonicBuilder::<English>::default()
            .phrase(phrase.trim())
            .index(index)
            .and_then(|builder| builder.build())
            .map_err(|e| Error::Env(format!("{env}: index {index}: {e}")))
        })
        .collect()
    }
    AccountSource::Key { env } => {
      let key = secret(env)?;
      let signer = key
        .trim()
        .parse::<PrivateKeySigner>()
        .map_err(|e| Error::Env(format!("{env}: {e}")))?;
      Ok(vec![signer])
    }
  }
}

/// Builds one signer per configured account, in config order, so index `i` is account `i`.
///
/// A missing or empty env var fails with `Env(name)`; two entries that resolve to the same
/// address fail, since two lanes would then share one nonce.
pub fn signers(sources: &[AccountSource]) -> Result<Vec<PrivateKeySigner>> {
  let mut seen = HashSet::new();
  let mut out = Vec::new();
  for source in sources {
    for signer in source_signers(source)? {
      if !seen.insert(signer.address()) {
        return Err(Error::Config(format!(
          "duplicate account {}",
          signer.address()
        )));
      }
      out.push(signer);
    }
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use alloy::primitives::{Address, address};

  use super::*;

  /// The anvil test mnemonic.
  const ANVIL: &str = "test test test test test test test test test test test junk";

  /// Sets `name` to `value` for one test.
  fn set(name: &str, value: &str) {
    // SAFETY: each test uses its own variable names, which no other code reads.
    unsafe { std::env::set_var(name, value) };
  }

  /// Indices 0 to 2 of the anvil mnemonic derive the documented addresses.
  #[test]
  fn derives_known_mnemonic_addresses() {
    set("ARB_TEST_MNEMONIC_KNOWN", ANVIL);
    let sources = [AccountSource::Mnemonic {
      env: "ARB_TEST_MNEMONIC_KNOWN".into(),
      indices: vec![0, 1, 2],
    }];
    let addresses: Vec<Address> = signers(&sources)
      .unwrap()
      .iter()
      .map(PrivateKeySigner::address)
      .collect();
    assert_eq!(
      addresses,
      vec![
        address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8"),
        address!("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
      ]
    );
  }

  /// A key that repeats a mnemonic account is rejected and the address is named.
  #[test]
  fn duplicate_account_rejected() {
    set("ARB_TEST_MNEMONIC_DUP", ANVIL);
    set(
      "ARB_TEST_KEY_DUP",
      "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    );
    let sources = [
      AccountSource::Mnemonic {
        env: "ARB_TEST_MNEMONIC_DUP".into(),
        indices: vec![0, 1],
      },
      AccountSource::Key {
        env: "ARB_TEST_KEY_DUP".into(),
      },
    ];
    let err = signers(&sources).unwrap_err();
    assert!(
      matches!(&err, Error::Config(why) if why == "duplicate account 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
      "{err}"
    );
  }

  /// An empty env var counts as missing, like an unset one.
  #[test]
  fn empty_env_var_is_missing() {
    set("ARB_TEST_KEY_EMPTY", "");
    let empty = [AccountSource::Key {
      env: "ARB_TEST_KEY_EMPTY".into(),
    }];
    assert!(matches!(signers(&empty), Err(Error::Env(name)) if name == "ARB_TEST_KEY_EMPTY"));
    let unset = [AccountSource::Mnemonic {
      env: "ARB_TEST_MNEMONIC_UNSET".into(),
      indices: vec![0],
    }];
    assert!(matches!(signers(&unset), Err(Error::Env(name)) if name == "ARB_TEST_MNEMONIC_UNSET"));
  }
}
