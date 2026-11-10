//! Batched contract reads through Multicall3.

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol_types::SolCall;

use crate::chain::evm::bindings::{Call3, IMulticall3, Result3};
use crate::error::{Error, Result};

/// Most calls sent in one `aggregate3`, so a large batch stays under the node's call gas cap.
const CHUNK: usize = 500;

/// One read of `call` on `target`, allowed to fail so the rest of the batch still answers.
pub fn read<C: SolCall>(target: Address, call: &C) -> Call3 {
  Call3 {
    target,
    allowFailure: true,
    callData: call.abi_encode().into(),
  }
}

/// Decodes one result as the return value of call `C`, failing when the call reverted.
pub fn decode<C: SolCall>(result: &Result3) -> Result<C::Return> {
  if !result.success {
    return Err(Error::Contract(format!(
      "{} failed in multicall",
      C::SIGNATURE
    )));
  }
  C::abi_decode_returns(&result.returnData).map_err(|e| Error::Contract(e.to_string()))
}

/// Sends `calls` through Multicall3 at `block`, in chunks of at most [`CHUNK`], and returns one
/// result per call in order.
pub async fn aggregate<P: Provider>(
  provider: &P,
  multicall: Address,
  block: BlockId,
  calls: Vec<Call3>,
) -> Result<Vec<Result3>> {
  let contract = IMulticall3::new(multicall, provider);
  let mut results = Vec::with_capacity(calls.len());
  for chunk in calls.chunks(CHUNK) {
    let answered = contract
      .aggregate3(chunk.to_vec())
      .block(block)
      .call()
      .await?;
    if answered.len() != chunk.len() {
      return Err(Error::Contract(format!(
        "multicall answered {} of {} calls",
        answered.len(),
        chunk.len()
      )));
    }
    results.extend(answered);
  }
  Ok(results)
}
