//! Batched contract reads through Multicall3.

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use futures::{StreamExt, TryStreamExt, stream};

use crate::chain::evm::bindings::{Call3, IMulticall3, Result3};
use crate::error::{Error, Result};

/// Most calls sent in one `aggregate3`, so a large batch stays under the node's call gas cap.
const CHUNK: usize = 500;
/// Most `aggregate3` requests one batch keeps in flight. A forked node fetches each storage
/// slot it lacks from upstream one at a time within a request, but serves requests in
/// parallel, so a cold batch split this many ways overlaps those fetches.
pub const CONCURRENCY: usize = 16;
/// Fewest calls a split request carries. A batch this small or smaller goes out whole, since
/// it costs a warm node less than the extra round trips splitting it would add.
const MIN_CHUNK: usize = 16;

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

/// Calls per request for a batch of `len` calls: spread over [`CONCURRENCY`] requests, but
/// never fewer than [`MIN_CHUNK`] nor more than [`CHUNK`].
pub(crate) fn chunk_size(len: usize) -> usize {
  len.div_ceil(CONCURRENCY).clamp(MIN_CHUNK, CHUNK)
}

/// Sends one chunk as a single `aggregate3` at `block`, failing unless every call is answered.
async fn send_chunk<P: Provider>(
  provider: &P,
  multicall: Address,
  block: BlockId,
  chunk: Vec<Call3>,
) -> Result<Vec<Result3>> {
  let sent = chunk.len();
  let answered = IMulticall3::new(multicall, provider)
    .aggregate3(chunk)
    .block(block)
    .call()
    .await?;
  if answered.len() != sent {
    return Err(Error::Contract(format!(
      "multicall answered {} of {sent} calls",
      answered.len()
    )));
  }
  Ok(answered)
}

/// Sends `calls` through Multicall3 at `block` and returns one result per call in order.
///
/// The calls go out in chunks of [`chunk_size`], up to [`CONCURRENCY`] at once. The answers
/// are joined in chunk order, so the result is the same as sending the chunks one by one,
/// and the first failed chunk fails the whole batch.
pub async fn aggregate<P: Provider>(
  provider: &P,
  multicall: Address,
  block: BlockId,
  calls: Vec<Call3>,
) -> Result<Vec<Result3>> {
  let size = chunk_size(calls.len());
  let chunks: Vec<Vec<Call3>> = calls.chunks(size).map(<[Call3]>::to_vec).collect();
  let answers: Vec<Vec<Result3>> = stream::iter(chunks)
    .map(|chunk| send_chunk(provider, multicall, block, chunk))
    .buffered(CONCURRENCY)
    .try_collect()
    .await?;
  Ok(answers.concat())
}

#[cfg(test)]
mod tests {
  use super::*;
  use alloy::providers::mock::Asserter;
  use alloy::sol_types::SolCall;

  use crate::chain::evm::bindings::IERC20;
  use crate::testkit::{mock_provider, push_multicall};

  /// Calls in the test batch: enough for three chunks.
  const CALLS: usize = 3 * MIN_CHUNK;

  /// `decimals()` reads of `CALLS` distinct tokens.
  fn calls() -> Vec<Call3> {
    (0..CALLS)
      .map(|i| read(Address::with_last_byte(i as u8), &IERC20::decimalsCall {}))
      .collect()
  }

  /// The answer for call `i`: its own index as the decimals.
  fn answer(i: usize) -> Option<Vec<u8>> {
    Some(IERC20::decimalsCall::abi_encode_returns(&(i as u8)))
  }

  /// Small batches go out whole; large ones split up to `CONCURRENCY` ways within the caps.
  #[test]
  fn chunk_size_spreads_within_caps() {
    assert_eq!(chunk_size(0), MIN_CHUNK);
    assert_eq!(chunk_size(MIN_CHUNK), MIN_CHUNK);
    assert_eq!(chunk_size(3_000), 188);
    assert_eq!(chunk_size(100_000), CHUNK);
  }

  /// A batch split into chunks returns every answer in call order, as one sequential
  /// multicall would.
  #[tokio::test]
  async fn chunked_answers_keep_call_order() {
    let asserter = Asserter::new();
    let answers: Vec<Option<Vec<u8>>> = (0..CALLS).map(answer).collect();
    for chunk in answers.chunks(chunk_size(CALLS)) {
      push_multicall(&asserter, chunk.to_vec());
    }
    let results = aggregate(
      &mock_provider(&asserter),
      Address::ZERO,
      BlockId::latest(),
      calls(),
    )
    .await
    .unwrap();
    assert!(asserter.read_q().is_empty(), "a chunk was not sent");
    let decoded: Vec<u8> = results
      .iter()
      .map(|r| decode::<IERC20::decimalsCall>(r).unwrap())
      .collect();
    assert_eq!(decoded, (0..CALLS as u8).collect::<Vec<_>>());
  }

  /// A failed request for one chunk fails the whole batch.
  #[tokio::test]
  async fn failed_chunk_fails_batch() {
    let asserter = Asserter::new();
    let size = chunk_size(CALLS);
    push_multicall(&asserter, (0..size).map(answer).collect());
    asserter.push_failure_msg("upstream reset");
    push_multicall(&asserter, (2 * size..CALLS).map(answer).collect());
    let result = aggregate(
      &mock_provider(&asserter),
      Address::ZERO,
      BlockId::latest(),
      calls(),
    )
    .await;
    assert!(result.is_err());
  }
}
