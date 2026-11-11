//! Conversions between chain neutral values and alloy types.

use alloy::primitives::Address;

use crate::error::{Error, Result};
use crate::redact::redact;
use crate::types::Id;

/// Length of an EVM address in bytes.
const ADDRESS_LEN: usize = 20;

/// Reads an [`Id`] as an address, rejecting any other length.
pub fn to_address(id: &Id) -> Result<Address> {
  let bytes = id.as_bytes();
  if bytes.len() != ADDRESS_LEN {
    return Err(Error::Config(format!(
      "id {id} is {} bytes, expected {ADDRESS_LEN}",
      bytes.len()
    )));
  }
  Ok(Address::from_slice(bytes))
}

/// Wraps an address as an [`Id`].
pub fn to_id(address: Address) -> Id {
  Id::new(address.as_slice().to_vec())
}

impl From<alloy::transports::TransportError> for Error {
  /// Maps a transport failure to an RPC error, redacted, since reqwest quotes the node URL
  /// and with it any key.
  fn from(e: alloy::transports::TransportError) -> Self {
    Self::Rpc(redact(&e.to_string()))
  }
}

impl From<alloy::contract::Error> for Error {
  /// Maps a contract call failure to a contract error, redacted, since it can wrap a
  /// transport error that quotes the node URL.
  fn from(e: alloy::contract::Error) -> Self {
    Self::Contract(redact(&e.to_string()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// An address survives the trip through an id.
  #[test]
  fn address_roundtrip() {
    let address = Address::repeat_byte(0x42);
    assert_eq!(to_address(&to_id(address)).unwrap(), address);
  }

  /// A transport error quoting a keyed node URL loses the key on its way into an
  /// [`Error`].
  #[test]
  fn transport_error_redacted() {
    let url = "https://node.example/v2/SECRETKEY";
    crate::redact::register(url);
    let transport = alloy::transports::TransportErrorKind::custom_str(&format!(
      "error sending request for url ({url})"
    ));
    let text = Error::from(transport).to_string();
    assert!(!text.contains("SECRETKEY"), "{text}");
    assert!(text.starts_with("rpc: "), "{text}");
  }

  /// An id that is not 20 bytes is not an address.
  #[test]
  fn short_id_rejected() {
    assert!(matches!(
      to_address(&Id::new(vec![1, 2, 3])),
      Err(Error::Config(_))
    ));
  }
}
