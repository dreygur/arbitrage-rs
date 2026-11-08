//! Conversions between chain neutral values and alloy types.

use alloy::primitives::Address;

use crate::error::{Error, Result};
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
  /// Maps a transport failure to an RPC error.
  fn from(e: alloy::transports::TransportError) -> Self {
    Self::Rpc(e.to_string())
  }
}

impl From<alloy::contract::Error> for Error {
  /// Maps a contract call failure to a contract error.
  fn from(e: alloy::contract::Error) -> Self {
    Self::Contract(e.to_string())
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

  /// An id that is not 20 bytes is not an address.
  #[test]
  fn short_id_rejected() {
    assert!(matches!(
      to_address(&Id::new(vec![1, 2, 3])),
      Err(Error::Config(_))
    ));
  }
}
