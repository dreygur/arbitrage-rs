mod uniswap;
use ethers::{
  contract::{abigen, ContractFactory},
  core::utils::Anvil,
  middleware::SignerMiddleware,
  providers::{Http, Provider},
  signers::{LocalWallet, Signer},
  solc::Solc,
};
use eyre::Result;

pub struct EthWallet {

}
