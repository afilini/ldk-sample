use crate::convert::{
	BlockchainInfo, FeeResponse, FundedTx, ListUnspentResponse, MempoolMinFeeResponse, NewAddress,
	RawTx, SignedTx,
};
use crate::disk::FilesystemLogger;
use crate::hex_utils;
use base64;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::{encode, Decodable, Encodable};
use bitcoin::hash_types::{BlockHash, Txid};
use bitcoin::hashes::Hash;
use bitcoin::util::address::{Address, Payload, WitnessVersion};
use bitcoin::{OutPoint, Script, TxOut, WPubkeyHash, XOnlyPublicKey};
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::log_error;
use lightning::routing::utxo::{UtxoLookup, UtxoResult};
use lightning::util::logger::Logger;
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource};
use serde_json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct BitcoindClient {
	pub(crate) bitcoind_rpc_client: Arc<RpcClient>,
	host: String,
	port: u16,
	rpc_user: String,
	rpc_password: String,
	fees: Arc<HashMap<ConfirmationTarget, AtomicU32>>,
	handle: tokio::runtime::Handle,
	logger: Arc<FilesystemLogger>,
}

impl BlockSource for BitcoindClient {
	fn get_header<'a>(
		&'a self, header_hash: &'a BlockHash, height_hint: Option<u32>,
	) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
		Box::pin(async move { self.bitcoind_rpc_client.get_header(header_hash, height_hint).await })
	}

	fn get_block<'a>(
		&'a self, header_hash: &'a BlockHash,
	) -> AsyncBlockSourceResult<'a, BlockData> {
		Box::pin(async move { self.bitcoind_rpc_client.get_block(header_hash).await })
	}

	fn get_best_block<'a>(&'a self) -> AsyncBlockSourceResult<(BlockHash, Option<u32>)> {
		Box::pin(async move { self.bitcoind_rpc_client.get_best_block().await })
	}
}

/// The minimum feerate we are allowed to send, as specify by LDK.
const MIN_FEERATE: u32 = 253;

impl BitcoindClient {
	pub(crate) async fn new(
		host: String, port: u16, rpc_user: String, rpc_password: String,
		handle: tokio::runtime::Handle, logger: Arc<FilesystemLogger>,
	) -> std::io::Result<Self> {
		let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
		let rpc_credentials =
			base64::encode(format!("{}:{}", rpc_user.clone(), rpc_password.clone()));
		let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint)?;
		let _dummy = bitcoind_rpc_client
			.call_method::<BlockchainInfo>("getblockchaininfo", &vec![])
			.await
			.map_err(|_| {
				std::io::Error::new(std::io::ErrorKind::PermissionDenied,
				"Failed to make initial call to bitcoind - please check your RPC user/password and access settings")
			})?;
		let mut fees: HashMap<ConfirmationTarget, AtomicU32> = HashMap::new();
		fees.insert(ConfirmationTarget::MempoolMinimum, AtomicU32::new(MIN_FEERATE));
		fees.insert(ConfirmationTarget::Background, AtomicU32::new(MIN_FEERATE));
		fees.insert(ConfirmationTarget::Normal, AtomicU32::new(2000));
		fees.insert(ConfirmationTarget::HighPriority, AtomicU32::new(5000));
		let client = Self {
			bitcoind_rpc_client: Arc::new(bitcoind_rpc_client),
			host,
			port,
			rpc_user,
			rpc_password,
			fees: Arc::new(fees),
			handle: handle.clone(),
			logger,
		};
		BitcoindClient::poll_for_fee_estimates(
			client.fees.clone(),
			client.bitcoind_rpc_client.clone(),
			handle,
		);
		Ok(client)
	}

	fn poll_for_fee_estimates(
		fees: Arc<HashMap<ConfirmationTarget, AtomicU32>>, rpc_client: Arc<RpcClient>,
		handle: tokio::runtime::Handle,
	) {
		handle.spawn(async move {
			loop {
				let mempoolmin_estimate = {
					let resp = rpc_client
						.call_method::<MempoolMinFeeResponse>("getmempoolinfo", &vec![])
						.await
						.unwrap();
					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => MIN_FEERATE,
					}
				};
				let background_estimate = {
					let background_conf_target = serde_json::json!(144);
					let background_estimate_mode = serde_json::json!("ECONOMICAL");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![background_conf_target, background_estimate_mode],
						)
						.await
						.unwrap();
					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => MIN_FEERATE,
					}
				};

				let normal_estimate = {
					let normal_conf_target = serde_json::json!(18);
					let normal_estimate_mode = serde_json::json!("ECONOMICAL");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![normal_conf_target, normal_estimate_mode],
						)
						.await
						.unwrap();
					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => 2000,
					}
				};

				let high_prio_estimate = {
					let high_prio_conf_target = serde_json::json!(6);
					let high_prio_estimate_mode = serde_json::json!("CONSERVATIVE");
					let resp = rpc_client
						.call_method::<FeeResponse>(
							"estimatesmartfee",
							&vec![high_prio_conf_target, high_prio_estimate_mode],
						)
						.await
						.unwrap();

					match resp.feerate_sat_per_kw {
						Some(feerate) => std::cmp::max(feerate, MIN_FEERATE),
						None => 5000,
					}
				};

				fees.get(&ConfirmationTarget::MempoolMinimum)
					.unwrap()
					.store(mempoolmin_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::Background)
					.unwrap()
					.store(background_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::Normal)
					.unwrap()
					.store(normal_estimate, Ordering::Release);
				fees.get(&ConfirmationTarget::HighPriority)
					.unwrap()
					.store(high_prio_estimate, Ordering::Release);
				tokio::time::sleep(Duration::from_secs(60)).await;
			}
		});
	}

	pub fn get_new_rpc_client(&self) -> std::io::Result<RpcClient> {
		let http_endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
		let rpc_credentials =
			base64::encode(format!("{}:{}", self.rpc_user.clone(), self.rpc_password.clone()));
		RpcClient::new(&rpc_credentials, http_endpoint)
	}

	pub async fn create_raw_transaction(&self, outputs: Vec<HashMap<String, f64>>) -> RawTx {
		let outputs_json = serde_json::json!(outputs);
		self.bitcoind_rpc_client
			.call_method::<RawTx>(
				"createrawtransaction",
				&vec![serde_json::json!([]), outputs_json],
			)
			.await
			.unwrap()
	}

	pub async fn fund_raw_transaction(&self, raw_tx: RawTx) -> FundedTx {
		let raw_tx_json = serde_json::json!(raw_tx.0);
		let options = serde_json::json!({
			// LDK gives us feerates in satoshis per KW but Bitcoin Core here expects fees
			// denominated in satoshis per vB. First we need to multiply by 4 to convert weight
			// units to virtual bytes, then divide by 1000 to convert KvB to vB.
			"fee_rate": self.get_est_sat_per_1000_weight(ConfirmationTarget::Normal) as f64 / 250.0,
			// While users could "cancel" a channel open by RBF-bumping and paying back to
			// themselves, we don't allow it here as its easy to have users accidentally RBF bump
			// and pay to the channel funding address, which results in loss of funds. Real
			// LDK-based applications should enable RBF bumping and RBF bump either to a local
			// change address or to a new channel output negotiated with the same node.
			"replaceable": false,
		});
		self.bitcoind_rpc_client
			.call_method("fundrawtransaction", &[raw_tx_json, options])
			.await
			.unwrap()
	}

	pub async fn send_raw_transaction(&self, raw_tx: RawTx) {
		let raw_tx_json = serde_json::json!(raw_tx.0);
		self.bitcoind_rpc_client
			.call_method::<Txid>("sendrawtransaction", &[raw_tx_json])
			.await
			.unwrap();
	}

	pub async fn sign_raw_transaction_with_wallet(&self, tx_hex: String) -> SignedTx {
		let tx_hex_json = serde_json::json!(tx_hex);
		self.bitcoind_rpc_client
			.call_method("signrawtransactionwithwallet", &vec![tx_hex_json])
			.await
			.unwrap()
	}

	pub async fn get_new_address(&self) -> Address {
		let addr_args = vec![serde_json::json!("LDK output address")];
		let addr = self
			.bitcoind_rpc_client
			.call_method::<NewAddress>("getnewaddress", &addr_args)
			.await
			.unwrap();
		Address::from_str(addr.0.as_str()).unwrap()
	}

	pub async fn get_blockchain_info(&self) -> BlockchainInfo {
		self.bitcoind_rpc_client
			.call_method::<BlockchainInfo>("getblockchaininfo", &vec![])
			.await
			.unwrap()
	}

	pub async fn list_unspent(&self) -> ListUnspentResponse {
		self.bitcoind_rpc_client
			.call_method::<ListUnspentResponse>("listunspent", &vec![])
			.await
			.unwrap()
	}
}

impl FeeEstimator for BitcoindClient {
	fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
		self.fees.get(&confirmation_target).unwrap().load(Ordering::Acquire)
	}
}

impl BroadcasterInterface for BitcoindClient {
	fn broadcast_transactions(&self, txs: &[&Transaction]) {
		// TODO: Rather than calling `sendrawtransaction` in a a loop, we should probably use
		// `submitpackage` once it becomes available.
		for tx in txs {
			let bitcoind_rpc_client = Arc::clone(&self.bitcoind_rpc_client);
			let tx_serialized = encode::serialize_hex(tx);
			let tx_json = serde_json::json!(tx_serialized);
			let logger = Arc::clone(&self.logger);
			self.handle.spawn(async move {
				// This may error due to RL calling `broadcast_transactions` with the same transaction
				// multiple times, but the error is safe to ignore.
				match bitcoind_rpc_client
					.call_method::<Txid>("sendrawtransaction", &vec![tx_json])
					.await
					{
						Ok(_) => {}
						Err(e) => {
							let err_str = e.get_ref().unwrap().to_string();
							log_error!(logger,
									   "Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\nTransaction: {}",
									   err_str,
									   tx_serialized);
							print!("Warning, failed to broadcast a transaction, this is likely okay but may indicate an error: {}\n> ", err_str);
						}
					}
			});
		}
	}
}

impl UtxoLookup for BitcoindClient {
	fn get_utxo(&self, _genesis_hash: &BlockHash, _short_channel_id: u64) -> UtxoResult {
		// P2PGossipSync takes None for a UtxoLookup, so this will never be called.
		todo!();
	}
}

impl WalletSource for BitcoindClient {
	fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
		let utxos = tokio::task::block_in_place(move || {
			self.handle.block_on(async move { self.list_unspent().await }).0
		});
		Ok(utxos
			.into_iter()
			.filter_map(|utxo| {
				let outpoint = OutPoint { txid: utxo.txid, vout: utxo.vout };
				match utxo.address.payload {
					Payload::WitnessProgram { version, ref program } => match version {
						WitnessVersion::V0 => WPubkeyHash::from_slice(program)
							.map(|wpkh| Utxo::new_v0_p2wpkh(outpoint, utxo.amount, &wpkh))
							.ok(),
						// TODO: Add `Utxo::new_v1_p2tr` upstream.
						WitnessVersion::V1 => XOnlyPublicKey::from_slice(program)
							.map(|_| Utxo {
								outpoint,
								output: TxOut {
									value: utxo.amount,
									script_pubkey: Script::new_witness_program(version, program),
								},
								satisfaction_weight: 1 /* empty script_sig */ * WITNESS_SCALE_FACTOR as u64 +
									1 /* witness items */ + 1 /* schnorr sig len */ + 64, /* schnorr sig */
							})
							.ok(),
						_ => None,
					},
					_ => None,
				}
			})
			.collect())
	}

	fn get_change_script(&self) -> Result<Script, ()> {
		tokio::task::block_in_place(move || {
			Ok(self.handle.block_on(async move { self.get_new_address().await.script_pubkey() }))
		})
	}

	fn sign_tx(&self, tx: Transaction) -> Result<Transaction, ()> {
		let mut tx_bytes = Vec::new();
		let _ = tx.consensus_encode(&mut tx_bytes).map_err(|_| ());
		let tx_hex = hex_utils::hex_str(&tx_bytes);
		let signed_tx = tokio::task::block_in_place(move || {
			self.handle.block_on(async move { self.sign_raw_transaction_with_wallet(tx_hex).await })
		});
		let signed_tx_bytes = hex_utils::to_vec(&signed_tx.hex).ok_or(())?;
		Transaction::consensus_decode(&mut signed_tx_bytes.as_slice()).map_err(|_| ())
	}
}
