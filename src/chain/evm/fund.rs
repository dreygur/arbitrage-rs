//! Fork funding: gives the test accounts ETH and the configured tokens on an anvil fork.
//!
//! Every anvil only method sits behind an `anvil_nodeInfo` probe, so pointing this at a real
//! node fails before anything is changed.

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use serde_json::Value;
use tracing::{info, warn};

use crate::chain::evm::bindings::IERC20;
use crate::chain::evm::config::{ForkSection, ForkToken};
use crate::error::{Error, Result};
use crate::types::Amount;

/// Anvil method that only an anvil node answers; the guard for everything below.
const NODE_INFO: &str = "anvil_nodeInfo";
/// Anvil method that sets an account's native balance.
const SET_BALANCE: &str = "anvil_setBalance";
/// Anvil method that gives an account tokens by writing the token's storage.
const DEAL_ERC20: &str = "anvil_dealERC20";
/// Anvil method that lets requests be sent from an address without its key.
const IMPERSONATE: &str = "anvil_impersonateAccount";
/// Anvil method that ends [`IMPERSONATE`].
const STOP_IMPERSONATING: &str = "anvil_stopImpersonatingAccount";
/// Anvil method that replaces an account's code.
const SET_CODE: &str = "anvil_setCode";
/// Code that clears an account's code.
const NO_CODE: &str = "0x";
/// Start of the code an EIP 7702 delegated account carries.
const DELEGATION_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];

/// What an account holds after funding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Funded {
  /// The funded account.
  pub address: Address,
  /// Its native balance.
  pub eth: Amount,
  /// Its balance of each configured token.
  pub tokens: Vec<(Address, Amount)>,
}

/// Clears the code at `address` when it is an EIP 7702 delegation, which on mainnet makes a
/// plain ETH transfer to it run out of gas.
async fn clear_delegation<P: Provider>(provider: &P, address: Address) -> Result<()> {
  let code = provider.get_code_at(address).await?;
  if code.starts_with(&DELEGATION_PREFIX) {
    provider
      .raw_request::<_, Value>(SET_CODE.into(), (address, NO_CODE))
      .await?;
    info!(%address, "cleared EIP 7702 delegation");
  }
  Ok(())
}

/// Moves `token.amount` of the token from its whale to `account`, impersonating the whale
/// and always ending the impersonation.
async fn transfer_from_whale<P: Provider>(
  provider: &P,
  account: Address,
  token: &ForkToken,
) -> Result<()> {
  provider
    .raw_request::<_, Value>(IMPERSONATE.into(), (token.whale,))
    .await?;
  let sent = async {
    let receipt = IERC20::new(token.address, provider)
      .transfer(account, token.amount)
      .from(token.whale)
      .send()
      .await?
      .get_receipt()
      .await
      .map_err(|e| Error::Contract(format!("whale transfer of {}: {e}", token.address)))?;
    if receipt.status() {
      Ok(())
    } else {
      Err(Error::Contract(format!(
        "whale transfer of {} reverted",
        token.address
      )))
    }
  }
  .await;
  let stopped = provider
    .raw_request::<_, Value>(STOP_IMPERSONATING.into(), (token.whale,))
    .await;
  sent?;
  stopped?;
  Ok(())
}

/// Gives `account` the token's configured amount, by writing storage and, when the node cannot,
/// by a transfer from the token's whale.
async fn give_token<P: Provider>(provider: &P, account: Address, token: &ForkToken) -> Result<()> {
  let dealt = provider
    .raw_request::<_, Value>(DEAL_ERC20.into(), (account, token.address, token.amount))
    .await;
  if let Err(e) = dealt {
    warn!(%account, token = %token.address, error = %e, "dealERC20 failed, transferring from the whale");
    transfer_from_whale(provider, account, token).await?;
  }
  Ok(())
}

/// Reads what `account` holds of ETH and of each configured token.
async fn balances<P: Provider>(
  provider: &P,
  fork: &ForkSection,
  account: Address,
) -> Result<Funded> {
  let eth = provider.get_balance(account).await?;
  let mut tokens = Vec::with_capacity(fork.tokens.len());
  for token in &fork.tokens {
    let held = IERC20::new(token.address, provider)
      .balanceOf(account)
      .call()
      .await?;
    tokens.push((token.address, held));
  }
  Ok(Funded {
    address: account,
    eth,
    tokens,
  })
}

/// Funds each of `accounts` with `fork.fund_eth` and every configured token, and returns what
/// each then holds.
///
/// `anvil_nodeInfo` goes first and any error becomes `Config("funding needs an anvil fork")`
/// with nothing else called. The `treasury` and every account lose an EIP 7702 delegation
/// first, since the mainnet account behind a test address may carry one.
pub async fn fund<P: Provider>(
  provider: &P,
  fork: &ForkSection,
  treasury: Address,
  accounts: &[Address],
) -> Result<Vec<Funded>> {
  provider
    .raw_request::<_, Value>(NODE_INFO.into(), ())
    .await
    .map_err(|_| Error::Config("funding needs an anvil fork".into()))?;
  clear_delegation(provider, treasury).await?;
  let mut funded = Vec::with_capacity(accounts.len());
  for &account in accounts {
    clear_delegation(provider, account).await?;
    provider
      .raw_request::<_, Value>(SET_BALANCE.into(), (account, U256::from(fork.fund_eth)))
      .await?;
    for token in &fork.tokens {
      give_token(provider, account, token).await?;
    }
    funded.push(balances(provider, fork, account).await?);
  }
  Ok(funded)
}

#[cfg(test)]
mod tests {
  use super::*;

  use alloy::primitives::Bytes;
  use alloy::providers::mock::Asserter;

  use crate::testkit::mock_provider;

  /// A fork section that funds one ETH and no tokens.
  fn fork() -> ForkSection {
    ForkSection {
      fund_eth: Amount::from(10u64).pow(Amount::from(18u64)),
      tokens: Vec::new(),
    }
  }

  /// The treasury stand in.
  fn treasury() -> Address {
    Address::repeat_byte(0x09)
  }

  /// A node that is not anvil is refused and no other request is made.
  #[tokio::test]
  async fn refuses_non_anvil() {
    let asserter = Asserter::new();
    asserter.push_failure_msg("method not found");
    asserter.push_success(&Bytes::new());
    let err = fund(&mock_provider(&asserter), &fork(), treasury(), &[])
      .await
      .unwrap_err();
    assert!(matches!(&err, Error::Config(m) if m == "funding needs an anvil fork"));
    assert_eq!(
      asserter.read_q().len(),
      1,
      "only anvil_nodeInfo may be sent"
    );
  }

  /// A treasury carrying 7702 delegation code gets `anvil_setCode`.
  #[tokio::test]
  async fn clears_delegated_treasury() {
    let asserter = Asserter::new();
    asserter.push_success(&serde_json::json!({}));
    let mut code = DELEGATION_PREFIX.to_vec();
    code.extend_from_slice(&[0x11; 20]);
    asserter.push_success(&Bytes::from(code));
    asserter.push_success(&Value::Null);
    fund(&mock_provider(&asserter), &fork(), treasury(), &[])
      .await
      .unwrap();
    assert!(asserter.read_q().is_empty(), "setCode was not called");
  }

  /// A treasury with no code is left alone.
  #[tokio::test]
  async fn leaves_plain_treasury() {
    let asserter = Asserter::new();
    asserter.push_success(&serde_json::json!({}));
    asserter.push_success(&Bytes::new());
    asserter.push_success(&Value::Null);
    fund(&mock_provider(&asserter), &fork(), treasury(), &[])
      .await
      .unwrap();
    assert_eq!(asserter.read_q().len(), 1, "setCode must not be called");
  }
}
