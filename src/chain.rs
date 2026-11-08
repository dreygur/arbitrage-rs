//! The only door between the chain neutral core and a network.

use std::sync::Arc;

use crate::config::Live;
use crate::error::Result;
use crate::types::{Amount, Control, Id, Opportunity, Status, Update, Verdict};
use crate::venue::Venue;

/// A network the bot reads state from and sends trades to.
#[allow(async_fn_in_trait)]
pub trait Chain: Send + Sync {
  /// Waits for the next state change and applies it before returning.
  async fn changes(&self) -> Result<Update>;

  /// Current pool snapshots, one venue per watched pool.
  fn snapshots(&self) -> Vec<Arc<dyn Venue>>;

  /// Gas cost of `op` in base token units, or `None` when the fee cap is exceeded.
  fn cost(&self, op: &Opportunity) -> Option<Amount>;

  /// Sends the opportunities, best first, and returns a verdict for each input, in any order, paired with the opportunity it concerns.
  async fn submit(&self, ops: Vec<Opportunity>) -> Vec<(Opportunity, Verdict)>;

  /// Applies new live settings in place.
  fn apply_live(&self, live: &Live);

  /// Delivers an operator command.
  fn control(&self, command: Control);

  /// Number of trades sent and not yet settled.
  fn in_flight(&self) -> usize;

  /// The token the bot borrows and profits in.
  fn base(&self) -> Id;

  /// Snapshot of the chain side for the dashboard.
  fn status(&self) -> Status;
}
