//! Bridge pool SDK functionality.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use borsh::BorshSerialize;
use ethbridge_bridge_contract::Bridge;
use ethers::providers::Middleware;
use futures::future::FutureExt;
use namada_core::ledger::eth_bridge::storage::bridge_pool::get_pending_key;
use namada_core::ledger::eth_bridge::storage::wrapped_erc20s;
use namada_core::types::ethereum_events::EthAddress;
use namada_core::types::key::common;
use namada_core::types::storage::Epoch;
use owo_colors::OwoColorize;
use serde::Serialize;

use super::{block_on_eth_sync, eth_sync_or_exit, BlockOnEthSync};
use crate::eth_bridge::ethers::abi::AbiDecode;
use crate::ledger::queries::{
    Client, GenBridgePoolProofReq, GenBridgePoolProofRsp, TransferToErcArgs,
    RPC,
};
use crate::proto::Tx;
use crate::sdk::args;
use crate::sdk::error::{
    EncodingError, Error, EthereumBridgeError, QueryError, TxError,
};
use crate::sdk::internal_macros::echo_error;
use crate::sdk::masp::{ShieldedContext, ShieldedUtils};
use crate::sdk::rpc::{
    query_storage_value, query_wasm_code_hash, validate_amount,
};
use crate::sdk::tx::prepare_tx;
use crate::sdk::wallet::{Wallet, WalletUtils};
use crate::types::address::{Address, InternalAddress};
use crate::types::control_flow::install_shutdown_signal;
use crate::types::control_flow::time::{Duration, Instant};
use crate::types::eth_abi::Encode;
use crate::types::eth_bridge_pool::{
    GasFee, PendingTransfer, TransferToEthereum, TransferToEthereumKind,
};
use crate::types::io::Io;
use crate::types::keccak::KeccakHash;
use crate::types::token::{balance_key, Amount, DenominatedAmount};
use crate::types::voting_power::FractionalVotingPower;
use crate::{display, display_line, edisplay_line};

/// Craft a transaction that adds a transfer to the Ethereum bridge pool.
pub async fn build_bridge_pool_tx<
    C: crate::ledger::queries::Client + Sync,
    U: WalletUtils,
    V: ShieldedUtils,
    IO: Io,
>(
    client: &C,
    wallet: &mut Wallet<U>,
    shielded: &mut ShieldedContext<V>,
    args::EthereumBridgePool {
        tx: tx_args,
        nut,
        asset,
        recipient,
        sender,
        amount,
        fee_amount,
        fee_payer,
        fee_token,
        code_path,
    }: args::EthereumBridgePool,
    wrapper_fee_payer: common::PublicKey,
) -> Result<(Tx, Option<Epoch>), Error> {
    let (transfer, tx_code_hash) = futures::try_join!(
        validate_bridge_pool_tx::<_, IO>(
            client,
            tx_args.force,
            nut,
            asset,
            recipient,
            sender,
            amount,
            fee_amount,
            fee_payer,
            fee_token,
        ),
        query_wasm_code_hash::<_, IO>(client, code_path.to_string_lossy()),
    )?;

    let chain_id = tx_args
        .chain_id
        .clone()
        .ok_or_else(|| Error::Other("No chain id available".into()))?;

    let mut tx = Tx::new(chain_id, tx_args.expiration);
    tx.add_code_from_hash(tx_code_hash).add_data(transfer);

    let epoch = prepare_tx::<C, U, V, IO>(
        client,
        wallet,
        shielded,
        &tx_args,
        &mut tx,
        wrapper_fee_payer,
        None,
    )
    .await?;

    Ok((tx, epoch))
}

/// Perform client validation checks on a Bridge pool transfer.
#[allow(clippy::too_many_arguments)]
async fn validate_bridge_pool_tx<C, IO>(
    client: &C,
    force: bool,
    nut: bool,
    asset: EthAddress,
    recipient: EthAddress,
    sender: Address,
    amount: args::InputAmount,
    fee_amount: args::InputAmount,
    fee_payer: Option<Address>,
    fee_token: Address,
) -> Result<PendingTransfer, Error>
where
    C: crate::ledger::queries::Client + Sync,
    IO: Io,
{
    let token_addr = wrapped_erc20s::token(&asset);
    let validate_token_amount =
        validate_amount::<_, IO>(client, amount, &token_addr, force).map(
            |result| {
                result.map_err(|e| {
                    Error::Other(format!(
                        "Failed to validate Bridge pool transfer amount: {e}"
                    ))
                })
            },
        );

    let validate_fee_amount = validate_amount::<_, IO>(
        client, fee_amount, &fee_token, force,
    )
    .map(|result| {
        result.map_err(|e| {
            Error::Other(format!(
                "Failed to validate Bridge pool fee amount: {e}",
            ))
        })
    });

    // validate amounts
    let (
        tok_denominated @ DenominatedAmount { amount, .. },
        fee_denominated @ DenominatedAmount {
            amount: fee_amount, ..
        },
    ) = futures::try_join!(validate_token_amount, validate_fee_amount)?;

    // build pending Bridge pool transfer
    let fee_payer = fee_payer.unwrap_or_else(|| sender.clone());
    let transfer = PendingTransfer {
        transfer: TransferToEthereum {
            asset,
            recipient,
            sender,
            amount,
            kind: if nut {
                TransferToEthereumKind::Nut
            } else {
                TransferToEthereumKind::Erc20
            },
        },
        gas_fee: GasFee {
            token: fee_token,
            amount: fee_amount,
            payer: fee_payer,
        },
    };

    if force {
        return Ok(transfer);
    }

    //======================================================
    // XXX: the following validations should be kept in sync
    // with the validations performed by the Bridge pool VP!
    //======================================================

    // check if an identical transfer is already in the Bridge pool
    let transfer_in_pool = RPC
        .shell()
        .storage_has_key(client, &get_pending_key(&transfer))
        .await
        .map_err(|e| Error::Query(QueryError::General(e.to_string())))?;
    if transfer_in_pool {
        return Err(Error::EthereumBridge(
            EthereumBridgeError::TransferAlreadyInPool,
        ));
    }

    let wnam_addr = RPC
        .shell()
        .eth_bridge()
        .read_native_erc20_contract(client)
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::RetrieveContract(
                e.to_string(),
            ))
        })?;

    // validate gas fee token
    match &transfer.gas_fee.token {
        Address::Internal(InternalAddress::Nut(_)) => {
            return Err(Error::EthereumBridge(
                EthereumBridgeError::InvalidFeeToken(transfer.gas_fee.token),
            ));
        }
        fee_token if fee_token == &wrapped_erc20s::token(&wnam_addr) => {
            return Err(Error::EthereumBridge(
                EthereumBridgeError::InvalidFeeToken(transfer.gas_fee.token),
            ));
        }
        _ => {}
    }

    // validate wnam token caps + whitelist
    if transfer.transfer.asset == wnam_addr {
        let flow_control = RPC
            .shell()
            .eth_bridge()
            .get_erc20_flow_control(client, &wnam_addr)
            .await
            .map_err(|e| {
                Error::Query(QueryError::General(format!(
                    "Failed to read wrapped NAM flow control data: {e}"
                )))
            })?;

        if !flow_control.whitelisted {
            return Err(Error::EthereumBridge(
                EthereumBridgeError::Erc20NotWhitelisted(wnam_addr),
            ));
        }

        if flow_control.exceeds_token_caps(transfer.transfer.amount) {
            return Err(Error::EthereumBridge(
                EthereumBridgeError::Erc20TokenCapsExceeded(wnam_addr),
            ));
        }
    }

    // validate balances
    let maybe_balance_error = if token_addr == transfer.gas_fee.token {
        let expected_debit = transfer.transfer.amount + transfer.gas_fee.amount;
        let balance: Amount = query_storage_value(
            client,
            &balance_key(&token_addr, &transfer.transfer.sender),
        )
        .await?;

        balance
            .checked_sub(expected_debit)
            .is_none()
            .then_some((token_addr, tok_denominated))
    } else {
        let check_tokens = async {
            let balance: Amount = query_storage_value(
                client,
                &balance_key(&token_addr, &transfer.transfer.sender),
            )
            .await?;
            Result::<_, Error>::Ok(
                balance
                    .checked_sub(transfer.transfer.amount)
                    .is_none()
                    .then_some((token_addr, tok_denominated)),
            )
        };
        let check_fees = async {
            let balance: Amount = query_storage_value(
                client,
                &balance_key(
                    &transfer.gas_fee.token,
                    &transfer.transfer.sender,
                ),
            )
            .await?;
            Result::<_, Error>::Ok(
                balance
                    .checked_sub(transfer.gas_fee.amount)
                    .is_none()
                    .then_some((
                        transfer.gas_fee.token.clone(),
                        fee_denominated,
                    )),
            )
        };

        let (err_tokens, err_fees) =
            futures::try_join!(check_tokens, check_fees)?;
        err_tokens.or(err_fees)
    };
    if let Some((token, amount)) = maybe_balance_error {
        return Err(Error::Tx(TxError::NegativeBalanceAfterTransfer(
            Box::new(transfer.transfer.sender),
            amount.to_string(),
            Box::new(token),
        )));
    }

    Ok(transfer)
}

/// A json serializable representation of the Ethereum
/// bridge pool.
#[derive(Serialize)]
struct BridgePoolResponse<'pool> {
    bridge_pool_contents: &'pool HashMap<String, PendingTransfer>,
}

/// Query the contents of the Ethereum bridge pool.
/// Prints out a json payload.
pub async fn query_bridge_pool<C, IO: Io>(
    client: &C,
) -> Result<HashMap<String, PendingTransfer>, Error>
where
    C: Client + Sync,
{
    let response: Vec<PendingTransfer> = RPC
        .shell()
        .eth_bridge()
        .read_ethereum_bridge_pool(client)
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::ReadBridgePool(
                e.to_string(),
            ))
        })?;
    let pool_contents: HashMap<String, PendingTransfer> = response
        .into_iter()
        .map(|transfer| (transfer.keccak256().to_string(), transfer))
        .collect();
    if pool_contents.is_empty() {
        display_line!(IO, "Bridge pool is empty.");
        return Ok(pool_contents);
    }
    let contents = BridgePoolResponse {
        bridge_pool_contents: &pool_contents,
    };
    display_line!(
        IO,
        "{}",
        serde_json::to_string_pretty(&contents)
            .map_err(|e| EncodingError::Serde(e.to_string()))?
    );
    Ok(pool_contents)
}

/// Query the contents of the Ethereum bridge pool that
/// is covered by the latest signed root.
/// Prints out a json payload.
pub async fn query_signed_bridge_pool<C, IO: Io>(
    client: &C,
) -> Result<HashMap<String, PendingTransfer>, Error>
where
    C: Client + Sync,
{
    let response: Vec<PendingTransfer> = RPC
        .shell()
        .eth_bridge()
        .read_signed_ethereum_bridge_pool(client)
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::ReadSignedBridgePool(
                e.to_string(),
            ))
        })?;
    let pool_contents: HashMap<String, PendingTransfer> = response
        .into_iter()
        .map(|transfer| (transfer.keccak256().to_string(), transfer))
        .collect();
    if pool_contents.is_empty() {
        display_line!(IO, "Bridge pool is empty.");
        return Ok(pool_contents);
    }
    let contents = BridgePoolResponse {
        bridge_pool_contents: &pool_contents,
    };
    display_line!(
        IO,
        "{}",
        serde_json::to_string_pretty(&contents)
            .map_err(|e| EncodingError::Serde(e.to_string()))?
    );
    Ok(pool_contents)
}

/// Iterates over all ethereum events
/// and returns the amount of voting power
/// backing each `TransferToEthereum` event.
///
/// Prints a json payload.
pub async fn query_relay_progress<C, IO: Io>(client: &C) -> Result<(), Error>
where
    C: Client + Sync,
{
    let resp = RPC
        .shell()
        .eth_bridge()
        .transfer_to_ethereum_progress(client)
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::TransferToEthProgress(
                e.to_string(),
            ))
        })?;
    display_line!(
        IO,
        "{}",
        serde_json::to_string_pretty(&resp)
            .map_err(|e| EncodingError::Serde(e.to_string()))?
    );
    Ok(())
}

/// Internal methdod to construct a proof that a set of transfers are in the
/// bridge pool.
async fn construct_bridge_pool_proof<C, IO: Io>(
    client: &C,
    args: GenBridgePoolProofReq<'_, '_>,
) -> Result<GenBridgePoolProofRsp, Error>
where
    C: Client + Sync,
{
    let in_progress = RPC
        .shell()
        .eth_bridge()
        .transfer_to_ethereum_progress(client)
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::TransferToEthProgress(
                e.to_string(),
            ))
        })?;

    let warnings: Vec<_> = in_progress
        .into_iter()
        .filter_map(|(ref transfer, voting_power)| {
            if voting_power >= FractionalVotingPower::ONE_THIRD {
                let hash = transfer.keccak256();
                args.transfers.contains(&hash).then_some(hash)
            } else {
                None
            }
        })
        .collect();

    if !warnings.is_empty() {
        let warning = "Warning".on_yellow();
        let warning = warning.bold();
        let warning = warning.blink();
        display_line!(
            IO,
            "{warning}: The following hashes correspond to transfers that \
             have surpassed the security threshold in Namada, therefore have \
             likely been relayed to Ethereum, but do not yet have a quorum of \
             validator signatures behind them in Namada; thus they are still \
             in the Bridge pool:\n{warnings:?}",
        );
        display!(IO, "\nDo you wish to proceed? (y/n): ");
        IO::flush();
        loop {
            let resp = IO::read().await.map_err(|e| {
                Error::Other(echo_error!(
                    IO,
                    "Encountered error reading from STDIN: {e:?}"
                ))
            })?;
            match resp.trim() {
                "y" => break,
                "n" => {
                    return Err(Error::Other(
                        "Aborted generating Bridge pool proof".into(),
                    ));
                }
                _ => {
                    display!(IO, "Expected 'y' or 'n'. Please try again: ");
                    IO::flush();
                }
            }
        }
    }

    let data = args.try_to_vec().unwrap();
    let response = RPC
        .shell()
        .eth_bridge()
        .generate_bridge_pool_proof(client, Some(data), None, false)
        .await
        .map_err(|e| {
            edisplay_line!(
                IO,
                "Encountered error constructing proof:\n{:?}",
                e
            );
            Error::EthereumBridge(EthereumBridgeError::GenBridgePoolProof(
                e.to_string(),
            ))
        })?;

    Ok(response.data)
}

/// A response from construction a bridge pool proof.
#[derive(Serialize)]
struct BridgePoolProofResponse {
    hashes: Vec<KeccakHash>,
    relayer_address: Address,
    total_fees: HashMap<Address, Amount>,
    abi_encoded_args: Vec<u8>,
}

/// Construct a merkle proof of a batch of transfers in
/// the bridge pool and return it to the user (as opposed
/// to relaying it to ethereum).
pub async fn construct_proof<C, IO: Io>(
    client: &C,
    args: args::BridgePoolProof,
) -> Result<(), Error>
where
    C: Client + Sync,
{
    let GenBridgePoolProofRsp {
        abi_encoded_args,
        appendices,
    } = construct_bridge_pool_proof::<_, IO>(
        client,
        GenBridgePoolProofReq {
            transfers: args.transfers.as_slice().into(),
            relayer: Cow::Borrowed(&args.relayer),
            with_appendix: true,
        },
    )
    .await?;
    let resp = BridgePoolProofResponse {
        hashes: args.transfers,
        relayer_address: args.relayer,
        total_fees: appendices
            .map(|appendices| {
                appendices.into_iter().fold(
                    HashMap::new(),
                    |mut total_fees, app| {
                        let GasFee { token, amount, .. } =
                            app.gas_fee.into_owned();
                        let fees = total_fees
                            .entry(token)
                            .or_insert_with(Amount::zero);
                        fees.receive(&amount);
                        total_fees
                    },
                )
            })
            .unwrap_or_default(),
        abi_encoded_args,
    };
    display_line!(
        IO,
        "{}",
        serde_json::to_string_pretty(&resp)
            .map_err(|e| EncodingError::Serde(e.to_string()))?
    );
    Ok(())
}

/// Relay a validator set update, signed off for a given epoch.
pub async fn relay_bridge_pool_proof<C, E, IO: Io>(
    eth_client: Arc<E>,
    nam_client: &C,
    args: args::RelayBridgePoolProof,
) -> Result<(), Error>
where
    C: Client + Sync,
    E: Middleware,
    E::Error: std::fmt::Debug + std::fmt::Display,
{
    let _signal_receiver = args.safe_mode.then(install_shutdown_signal);

    if args.sync {
        block_on_eth_sync::<_, IO>(
            &*eth_client,
            BlockOnEthSync {
                deadline: Instant::now() + Duration::from_secs(60),
                delta_sleep: Duration::from_secs(1),
            },
        )
        .await?;
    } else {
        eth_sync_or_exit::<_, IO>(&*eth_client).await?;
    }

    let GenBridgePoolProofRsp {
        abi_encoded_args, ..
    } = construct_bridge_pool_proof::<_, IO>(
        nam_client,
        GenBridgePoolProofReq {
            transfers: Cow::Owned(args.transfers),
            relayer: Cow::Owned(args.relayer),
            with_appendix: false,
        },
    )
    .await?;
    let bridge = match RPC
        .shell()
        .eth_bridge()
        .read_bridge_contract(nam_client)
        .await
    {
        Ok(address) => Bridge::new(address.address, eth_client),
        Err(err_msg) => {
            let error = "Error".on_red();
            let error = error.bold();
            let error = error.blink();
            display_line!(
                IO,
                "{error}: Failed to retrieve the Ethereum Bridge smart \
                 contract address from storage with \
                 reason:\n{err_msg}\n\nPerhaps the Ethereum bridge is not \
                 active.",
            );
            return Err(Error::EthereumBridge(
                EthereumBridgeError::RetrieveContract(err_msg.to_string()),
            ));
        }
    };

    let (validator_set, signatures, bp_proof): TransferToErcArgs =
        AbiDecode::decode(&abi_encoded_args).map_err(|error| {
            EncodingError::Decoding(echo_error!(
                IO,
                "Unable to decode the generated proof: {:?}",
                error
            ))
        })?;

    // NOTE: this operation costs no gas on Ethereum
    let contract_nonce = bridge
        .transfer_to_erc_20_nonce()
        .call()
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::ContractCall(
                e.to_string(),
            ))
        })?;

    match bp_proof.batch_nonce.cmp(&contract_nonce) {
        Ordering::Equal => {}
        Ordering::Less => {
            let error = "Error".on_red();
            let error = error.bold();
            let error = error.blink();
            display_line!(
                IO,
                "{error}: The Bridge pool nonce in the smart contract is \
                 {contract_nonce}, while the nonce in Namada is still {}. A \
                 relay of the former one has already happened, but a proof \
                 has yet to be crafted in Namada.",
                bp_proof.batch_nonce
            );
            return Err(Error::EthereumBridge(
                EthereumBridgeError::InvalidBpNonce,
            ));
        }
        Ordering::Greater => {
            let error = "Error".on_red();
            let error = error.bold();
            let error = error.blink();
            display_line!(
                IO,
                "{error}: The Bridge pool nonce in the smart contract is \
                 {contract_nonce}, while the nonce in Namada is still {}. \
                 Somehow, Namada's nonce is ahead of the contract's nonce!",
                bp_proof.batch_nonce
            );
            return Err(Error::EthereumBridge(
                EthereumBridgeError::InvalidBpNonce,
            ));
        }
    }

    let mut relay_op =
        bridge.transfer_to_erc(validator_set, signatures, bp_proof);
    if let Some(gas) = args.gas {
        relay_op.tx.set_gas(gas);
    }
    if let Some(gas_price) = args.gas_price {
        relay_op.tx.set_gas_price(gas_price);
    }
    if let Some(eth_addr) = args.eth_addr {
        relay_op.tx.set_from(eth_addr.into());
    }

    let pending_tx = relay_op.send().await.map_err(|e| {
        Error::EthereumBridge(EthereumBridgeError::ContractCall(e.to_string()))
    })?;
    let transf_result = pending_tx
        .confirmations(args.confirmations as usize)
        .await
        .map_err(|e| {
            Error::EthereumBridge(EthereumBridgeError::Rpc(e.to_string()))
        })?;

    display_line!(IO, "{transf_result:?}");
    Ok(())
}

mod recommendations {
    use std::collections::BTreeSet;

    use borsh::BorshDeserialize;
    use namada_core::types::uint::{self, Uint, I256};

    use super::*;
    use crate::edisplay_line;
    use crate::eth_bridge::storage::bridge_pool::{
        get_nonce_key, get_signed_root_key,
    };
    use crate::eth_bridge::storage::proof::BridgePoolRootProof;
    use crate::types::ethereum_events::Uint as EthUint;
    use crate::types::io::Io;
    use crate::types::storage::BlockHeight;
    use crate::types::vote_extensions::validator_set_update::{
        EthAddrBook, VotingPowersMap, VotingPowersMapExt,
    };

    const fn unsigned_transfer_fee() -> Uint {
        Uint::from_u64(37_500_u64)
    }

    const fn transfer_fee() -> I256 {
        I256(unsigned_transfer_fee())
    }

    const fn signature_fee() -> Uint {
        Uint::from_u64(24_500)
    }

    const fn valset_fee() -> Uint {
        Uint::from_u64(2000)
    }

    /// The different states while trying to solve
    /// for a recommended batch of transfers.
    struct AlgorithState {
        /// We are scanning transfers that increase
        /// net profits to the relayer. However, we
        /// are not in the feasible region.
        profitable: bool,
        /// We are scanning solutions that satisfy the
        /// requirements of the input.
        feasible_region: bool,
    }

    /// The algorithm exhibits two different remmondation strategies
    /// depending on whether the user is will to accept a positive cost
    /// for relaying.
    #[derive(PartialEq)]
    enum AlgorithmMode {
        /// Only keep profitable transactions
        Greedy,
        /// Allow transactions which are not profitable
        Generous,
    }

    /// Transfer to Ethereum that is eligible to be recommended
    /// for a relay operation, generating a profit.
    ///
    /// This means that the underlying Ethereum event has not
    /// been "seen" yet, and that the user provided appropriate
    /// conversion rates to gwei for the gas fee token in
    /// the transfer.
    #[derive(Debug, Eq, PartialEq)]
    struct EligibleRecommendation {
        /// Pending transfer to Ethereum.
        pending_transfer: PendingTransfer,
        /// Hash of the [`PendingTransfer`].
        transfer_hash: String,
        /// Cost of relaying the transfer, in gwei.
        cost: I256,
    }

    /// Batch of recommended transfers to Ethereum that generate
    /// a profit after a relay operation.
    #[derive(Debug, Eq, PartialEq)]
    struct RecommendedBatch {
        /// Hashes of the recommended transfers to be relayed.
        transfer_hashes: Vec<String>,
        /// Estimate of the total fees, measured in gwei, that will be paid
        /// on Ethereum.
        ethereum_gas_fees: Uint,
        /// Net profitt in gwei, based on the conversion rates provided
        /// to the algorithm.
        net_profit: I256,
        /// Gas fees paid by the transfers considered for relaying,
        /// paid in various token types.
        bridge_pool_gas_fees: HashMap<String, Uint>,
    }

    /// Recommend the most economical batch of transfers to relay based
    /// on a conversion rate estimates from NAM to ETH and gas usage
    /// heuristics.
    pub async fn recommend_batch<C, IO: Io>(
        client: &C,
        args: args::RecommendBatch,
    ) -> Result<(), Error>
    where
        C: Client + Sync,
    {
        // get transfers that can already been relayed but are awaiting a quorum
        // of backing votes.
        let in_progress = RPC
            .shell()
            .eth_bridge()
            .transfer_to_ethereum_progress(client)
            .await
            .map_err(|e| {
                Error::EthereumBridge(
                    EthereumBridgeError::TransferToEthProgress(e.to_string()),
                )
            })?
            .into_keys()
            .map(|pending| pending.keccak256().to_string())
            .collect::<BTreeSet<_>>();

        // get the signed bridge pool root so we can analyze the signatures
        // the estimate the gas cost of verifying them.
        let (bp_root, height) =
            <(BridgePoolRootProof, BlockHeight)>::try_from_slice(
                &RPC.shell()
                    .storage_value(
                        client,
                        None,
                        None,
                        false,
                        &get_signed_root_key(),
                    )
                    .await
                    .map_err(|err| {
                        Error::Query(QueryError::General(echo_error!(
                            IO,
                            "Failed to query Bridge pool proof: {err}"
                        )))
                    })?
                    .data,
            )
            .map_err(|err| {
                Error::Encode(EncodingError::Decoding(echo_error!(
                    IO,
                    "Failed to decode Bridge pool proof: {err}"
                )))
            })?;

        // get the latest bridge pool nonce
        let latest_bp_nonce = EthUint::try_from_slice(
            &RPC.shell()
                .storage_value(client, None, None, false, &get_nonce_key())
                .await
                .map_err(|err| {
                    Error::Query(QueryError::General(echo_error!(
                        IO,
                        "Failed to query Bridge pool nonce: {err}"
                    )))
                })?
                .data,
        )
        .map_err(|err| {
            Error::Encode(EncodingError::Decoding(echo_error!(
                IO,
                "Failed to decode Bridge pool nonce: {err}"
            )))
        })?;

        if latest_bp_nonce != bp_root.data.1 {
            edisplay_line!(
                IO,
                "The signed Bridge pool nonce is not up to date, repeat this \
                 query at a later time"
            );
            return Err(Error::EthereumBridge(
                EthereumBridgeError::InvalidBpNonce,
            ));
        }

        // Get the voting powers of each of validator who signed
        // the above root.
        let voting_powers = RPC
            .shell()
            .eth_bridge()
            .voting_powers_at_height(client, &height)
            .await
            .map_err(|e| {
                Error::EthereumBridge(EthereumBridgeError::QueryVotingPowers(
                    e.to_string(),
                ))
            })?;
        let valset_size = Uint::from_u64(voting_powers.len() as u64);

        // This is the gas cost for hashing the validator set and
        // checking a quorum of signatures (in gwei).
        let validator_gas = signature_fee()
            * signature_checks(voting_powers, &bp_root.signatures)
            + valset_fee() * valset_size;

        // we don't recommend transfers that have already been relayed
        let eligible = generate_eligible::<IO>(
            &args.conversion_table,
            &in_progress,
            query_signed_bridge_pool::<_, IO>(client).await?,
        )?;

        let max_gas =
            args.max_gas.map(Uint::from_u64).unwrap_or(uint::MAX_VALUE);
        let max_cost = args.gas.map(I256::from).unwrap_or_default();

        generate_recommendations::<IO>(
            eligible,
            &args.conversion_table,
            validator_gas,
            max_gas,
            max_cost,
        )?
        .map(
            |RecommendedBatch {
                 transfer_hashes,
                 ethereum_gas_fees,
                 net_profit,
                 bridge_pool_gas_fees,
             }| {
                display_line!(IO, "Recommended batch: {transfer_hashes:#?}");
                display_line!(
                    IO,
                    "Estimated Ethereum transaction gas (in gwei): \
                     {ethereum_gas_fees}",
                );
                display_line!(
                    IO,
                    "Estimated net profit (in gwei): {net_profit}"
                );
                display_line!(IO, "Total fees: {bridge_pool_gas_fees:#?}");
            },
        )
        .unwrap_or_else(|| {
            display_line!(
                IO,
                "Unable to find a recommendation satisfying the input \
                 parameters."
            );
        });

        Ok(())
    }

    /// Given an ordered list of signatures, figure out the size of the first
    /// subset constituting a 2 / 3 majority.
    ///
    /// The function is generic to make unit testing easier (otherwise a dev
    /// dependency needs to be added).
    fn signature_checks<T>(
        voting_powers: VotingPowersMap,
        sigs: &HashMap<EthAddrBook, T>,
    ) -> Uint {
        let voting_powers = voting_powers.get_sorted();
        let total_power = voting_powers.iter().map(|(_, &y)| y).sum::<Amount>();

        // Find the total number of signature checks Ethereum will make
        let mut power = FractionalVotingPower::NULL;
        Uint::from_u64(
            voting_powers
                .iter()
                .filter_map(|(a, &p)| sigs.get(a).map(|_| p))
                .take_while(|p| {
                    if power <= FractionalVotingPower::TWO_THIRDS {
                        power += FractionalVotingPower::new(
                            (*p).into(),
                            total_power.into(),
                        )
                        // NB: this unwrap is infallible, since we calculate
                        // the total voting power beforehand. the fraction's
                        // value will never exceed 1.0
                        .unwrap();
                        true
                    } else {
                        false
                    }
                })
                .count() as u64,
        )
    }

    /// Generate eligible recommendations.
    fn generate_eligible<IO: Io>(
        conversion_table: &HashMap<Address, args::BpConversionTableEntry>,
        in_progress: &BTreeSet<String>,
        signed_pool: HashMap<String, PendingTransfer>,
    ) -> Result<Vec<EligibleRecommendation>, Error> {
        let mut eligible: Vec<_> = signed_pool
            .into_iter()
            .filter_map(|(pending_hash, pending)| {
                if in_progress.contains(&pending_hash) {
                    return None;
                }

                let conversion_rate = conversion_table
                    .get(&pending.gas_fee.token)
                    .and_then(|entry| match entry.conversion_rate {
                        r if r == 0.0f64 => {
                            edisplay_line!(
                                IO,
                                "{}: Ignoring null conversion rate",
                                pending.gas_fee.token,
                            );
                            None
                        }
                        r if r < 0.0f64 => {
                            edisplay_line!(
                                IO,
                                "{}: Ignoring negative conversion rate: {r:.1}",
                                pending.gas_fee.token,
                            );
                            None
                        }
                        r if r > 1e9 => {
                            edisplay_line!(
                                IO,
                                "{}: Ignoring high conversion rate: {r:.1} > \
                                 10^9",
                                pending.gas_fee.token,
                            );
                            None
                        }
                        r => Some(r),
                    })?;

                // This is the amount of gwei a single gas token is worth
                let gwei_per_gas_token =
                    Uint::from_u64((1e9 / conversion_rate).floor() as u64);

                Some(
                    Uint::from(pending.gas_fee.amount)
                        .checked_mul(gwei_per_gas_token)
                        .ok_or_else(|| {
                            "Overflowed calculating earned gwei".into()
                        })
                        .and_then(I256::try_from)
                        .map_err(|err| err.to_string())
                        .and_then(|amt_of_earned_gwei| {
                            transfer_fee()
                                .checked_sub(&amt_of_earned_gwei)
                                .ok_or_else(|| {
                                    "Underflowed calculating relaying cost"
                                        .into()
                                })
                        })
                        .map(|cost| EligibleRecommendation {
                            cost,
                            pending_transfer: pending,
                            transfer_hash: pending_hash,
                        }),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                Error::EthereumBridge(EthereumBridgeError::RelayCost(
                    echo_error!(IO, "Failed to calculate relaying cost: {err}"),
                ))
            })?;

        // sort transfers in increasing amounts of profitability
        eligible.sort_by_key(|EligibleRecommendation { cost, .. }| *cost);

        Ok(eligible)
    }

    /// Generates the actual recommendation from restrictions given by the
    /// input parameters.
    fn generate_recommendations<IO: Io>(
        contents: Vec<EligibleRecommendation>,
        conversion_table: &HashMap<Address, args::BpConversionTableEntry>,
        validator_gas: Uint,
        max_gas: Uint,
        max_cost: I256,
    ) -> Result<Option<RecommendedBatch>, Error> {
        let mut state = AlgorithState {
            profitable: true,
            feasible_region: false,
        };

        let mode = if max_cost <= I256::zero() {
            AlgorithmMode::Greedy
        } else {
            AlgorithmMode::Generous
        };

        let mut total_gas = validator_gas;
        let mut total_cost = I256::try_from(validator_gas).map_err(|err| {
            Error::Encode(EncodingError::Conversion(echo_error!(
                IO,
                "Failed to convert value to I256: {err}"
            )))
        })?;
        let mut total_fees = HashMap::new();
        let mut recommendation = vec![];
        for EligibleRecommendation {
            cost,
            transfer_hash: hash,
            pending_transfer: transfer,
        } in contents.into_iter()
        {
            let next_total_gas = total_gas + unsigned_transfer_fee();
            let next_total_cost = total_cost + cost;
            if cost.is_negative() {
                if next_total_gas <= max_gas && next_total_cost <= max_cost {
                    state.feasible_region = true;
                } else if state.feasible_region {
                    // once we leave the feasible region, we will never re-enter
                    // it.
                    break;
                }
                recommendation.push(hash);
            } else if mode == AlgorithmMode::Generous {
                state.profitable = false;
                let is_feasible =
                    next_total_gas <= max_gas && next_total_cost <= max_cost;
                // once we leave the feasible region, we will never re-enter it.
                if state.feasible_region && !is_feasible {
                    break;
                } else {
                    recommendation.push(hash);
                }
            } else {
                break;
            }
            total_cost = next_total_cost;
            total_gas = next_total_gas;
            update_total_fees(&mut total_fees, transfer, conversion_table);
        }

        Ok(if state.feasible_region && !recommendation.is_empty() {
            Some(RecommendedBatch {
                transfer_hashes: recommendation,
                ethereum_gas_fees: total_gas,
                net_profit: -total_cost,
                bridge_pool_gas_fees: total_fees,
            })
        } else {
            edisplay_line!(
                IO,
                "Unable to find a recommendation satisfying the input \
                 parameters."
            );
            None
        })
    }

    fn update_total_fees(
        total_fees: &mut HashMap<String, Uint>,
        transfer: PendingTransfer,
        conversion_table: &HashMap<Address, args::BpConversionTableEntry>,
    ) {
        let GasFee { token, amount, .. } = transfer.gas_fee;
        let fees = total_fees
            .entry(
                conversion_table
                    .get(&token)
                    .map(|entry| entry.alias.clone())
                    .unwrap_or_else(|| token.to_string()),
            )
            .or_insert(uint::ZERO);
        *fees += Uint::from(amount);
    }

    #[cfg(test)]
    mod test_recommendations {
        use namada_core::types::address::Address;

        use super::*;
        use crate::types::io::DefaultIo;

        /// An established user address for testing & development
        pub fn bertha_address() -> Address {
            Address::decode(
                "atest1v4ehgw36xvcyyvejgvenxs34g3zygv3jxqunjd6rxyeyys3sxy6rwvfkx4qnj33hg9qnvse4lsfctw",
            )
            .expect("The token address decoding shouldn't fail")
        }

        /// Generate a pending transfer with the specified gas
        /// fee.
        pub fn transfer(gas_amount: u64) -> PendingTransfer {
            PendingTransfer {
                transfer: TransferToEthereum {
                    kind: TransferToEthereumKind::Erc20,
                    asset: EthAddress([1; 20]),
                    recipient: EthAddress([2; 20]),
                    sender: bertha_address(),
                    amount: Default::default(),
                },
                gas_fee: GasFee {
                    token: namada_core::types::address::nam(),
                    amount: gas_amount.into(),
                    payer: bertha_address(),
                },
            }
        }

        /// Convert transfers into a format that the
        /// [`generate_recommendations`] function understands.
        fn process_transfers(
            transfers: Vec<PendingTransfer>,
        ) -> Vec<EligibleRecommendation> {
            transfers
                .into_iter()
                .map(|t| EligibleRecommendation {
                    cost: transfer_fee() - t.gas_fee.amount.change(),
                    transfer_hash: t.keccak256().to_string(),
                    pending_transfer: t,
                })
                .collect()
        }

        fn address_book(i: u8) -> EthAddrBook {
            EthAddrBook {
                hot_key_addr: EthAddress([i; 20]),
                cold_key_addr: EthAddress([i; 20]),
            }
        }

        /// Data to pass to the [`test_generate_eligible_aux`] callback.
        struct TestGenerateEligible<'a> {
            pending: &'a PendingTransfer,
            conversion_table:
                &'a mut HashMap<Address, args::BpConversionTableEntry>,
            in_progress: &'a mut BTreeSet<String>,
            signed_pool: &'a mut HashMap<String, PendingTransfer>,
            expected_eligible: &'a mut Vec<EligibleRecommendation>,
        }

        impl TestGenerateEligible<'_> {
            /// Add ETH to a conversion table.
            fn add_eth_to_conversion_table(&mut self) {
                self.conversion_table.insert(
                    namada_core::types::address::eth(),
                    args::BpConversionTableEntry {
                        alias: "ETH".into(),
                        conversion_rate: 1e9, // 1 ETH = 1e9 GWEI
                    },
                );
            }
        }

        /// Helper function to test [`generate_eligible`].
        fn test_generate_eligible_aux<F>(
            mut callback: F,
        ) -> Vec<EligibleRecommendation>
        where
            F: FnMut(TestGenerateEligible<'_>),
        {
            let pending = PendingTransfer {
                transfer: TransferToEthereum {
                    kind: TransferToEthereumKind::Erc20,
                    asset: EthAddress([1; 20]),
                    recipient: EthAddress([2; 20]),
                    sender: bertha_address(),
                    amount: Default::default(),
                },
                gas_fee: GasFee {
                    token: namada_core::types::address::eth(),
                    amount: 1_000_000_000_u64.into(), // 1 GWEI
                    payer: bertha_address(),
                },
            };
            let mut table = HashMap::new();
            let mut in_progress = BTreeSet::new();
            let mut signed_pool = HashMap::new();
            let mut expected = vec![];
            callback(TestGenerateEligible {
                pending: &pending,
                conversion_table: &mut table,
                in_progress: &mut in_progress,
                signed_pool: &mut signed_pool,
                expected_eligible: &mut expected,
            });
            let eligible = generate_eligible::<DefaultIo>(
                &table,
                &in_progress,
                signed_pool,
            )
            .unwrap();
            assert_eq!(eligible, expected);
            eligible
        }

        /// Test the happy path of generating eligible recommendations
        /// for Bridge pool relayed transfers.
        #[test]
        fn test_generate_eligible_happy_path() {
            test_generate_eligible_aux(|mut ctx| {
                ctx.add_eth_to_conversion_table();
                ctx.signed_pool.insert(
                    ctx.pending.keccak256().to_string(),
                    ctx.pending.clone(),
                );
                ctx.expected_eligible.push(EligibleRecommendation {
                    transfer_hash: ctx.pending.keccak256().to_string(),
                    cost: transfer_fee()
                        - I256::try_from(ctx.pending.gas_fee.amount)
                            .expect("Test failed"),
                    pending_transfer: ctx.pending.clone(),
                });
            });
        }

        /// Test that a transfer is not recommended if it
        /// is in the process of being relayed (has >0 voting
        /// power behind it).
        #[test]
        fn test_generate_eligible_with_in_progress() {
            test_generate_eligible_aux(|mut ctx| {
                ctx.add_eth_to_conversion_table();
                ctx.signed_pool.insert(
                    ctx.pending.keccak256().to_string(),
                    ctx.pending.clone(),
                );
                ctx.in_progress.insert(ctx.pending.keccak256().to_string());
            });
        }

        /// Test that a transfer is not recommended if its gas
        /// token is not found in the conversion table.
        #[test]
        fn test_generate_eligible_no_gas_token() {
            test_generate_eligible_aux(|ctx| {
                ctx.signed_pool.insert(
                    ctx.pending.keccak256().to_string(),
                    ctx.pending.clone(),
                );
            });
        }

        #[test]
        fn test_signature_count() {
            let voting_powers = VotingPowersMap::from([
                (address_book(1), Amount::from(5)),
                (address_book(2), Amount::from(1)),
                (address_book(3), Amount::from(1)),
            ]);
            let signatures = HashMap::from([
                (address_book(1), 0),
                (address_book(2), 0),
                (address_book(3), 0),
            ]);
            let checks = signature_checks(voting_powers, &signatures);
            assert_eq!(checks, uint::ONE)
        }

        #[test]
        fn test_signature_count_with_skips() {
            let voting_powers = VotingPowersMap::from([
                (address_book(1), Amount::from(5)),
                (address_book(2), Amount::from(5)),
                (address_book(3), Amount::from(1)),
                (address_book(4), Amount::from(1)),
            ]);
            let signatures = HashMap::from([
                (address_book(1), 0),
                (address_book(3), 0),
                (address_book(4), 0),
            ]);
            let checks = signature_checks(voting_powers, &signatures);
            assert_eq!(checks, Uint::from_u64(3))
        }

        #[test]
        fn test_only_profitable() {
            let profitable = vec![transfer(100_000); 17];
            let hash = profitable[0].keccak256().to_string();
            let expected = vec![hash; 17];
            let recommendation = generate_recommendations::<DefaultIo>(
                process_transfers(profitable),
                &Default::default(),
                Uint::from_u64(800_000),
                uint::MAX_VALUE,
                I256::zero(),
            )
            .unwrap()
            .expect("Test failed")
            .transfer_hashes;
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_non_profitable_removed() {
            let mut transfers = vec![transfer(100_000); 17];
            let hash = transfers[0].keccak256().to_string();
            transfers.push(transfer(0));
            let expected: Vec<_> = vec![hash; 17];
            let recommendation = generate_recommendations::<DefaultIo>(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(800_000),
                uint::MAX_VALUE,
                I256::zero(),
            )
            .unwrap()
            .expect("Test failed")
            .transfer_hashes;
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_max_gas() {
            let transfers = vec![transfer(75_000); 4];
            let hash = transfers[0].keccak256().to_string();
            let expected = vec![hash; 2];
            let recommendation = generate_recommendations::<DefaultIo>(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(50_000),
                Uint::from_u64(150_000),
                I256(uint::MAX_SIGNED_VALUE),
            )
            .unwrap()
            .expect("Test failed")
            .transfer_hashes;
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_net_loss() {
            let mut transfers = vec![transfer(75_000); 4];
            transfers.extend([transfer(17_500), transfer(17_500)]);
            let expected: Vec<_> = transfers
                .iter()
                .map(|t| t.keccak256().to_string())
                .take(5)
                .collect();
            let recommendation = generate_recommendations::<DefaultIo>(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(150_000),
                uint::MAX_VALUE,
                I256::from(20_000),
            )
            .unwrap()
            .expect("Test failed")
            .transfer_hashes;
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_net_loss_max_gas() {
            let mut transfers = vec![transfer(75_000); 4];
            let hash = transfers[0].keccak256().to_string();
            let expected = vec![hash; 4];
            transfers.extend([transfer(17_500), transfer(17_500)]);
            let recommendation = generate_recommendations::<DefaultIo>(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(150_000),
                Uint::from_u64(330_000),
                I256::from(20_000),
            )
            .unwrap()
            .expect("Test failed")
            .transfer_hashes;
            assert_eq!(recommendation, expected);
        }

        #[test]
        fn test_wholly_infeasible() {
            let transfers = vec![transfer(75_000); 4];
            let recommendation = generate_recommendations::<DefaultIo>(
                process_transfers(transfers),
                &Default::default(),
                Uint::from_u64(300_000),
                uint::MAX_VALUE,
                I256::from(20_000),
            )
            .unwrap();
            assert!(recommendation.is_none())
        }

        /// Test the profit margin obtained from relaying two
        /// Bridge pool transfers with two distinct token types,
        /// whose relation is 1:2 in value.
        #[test]
        fn test_conversion_table_profit_margin() {
            // apfel is worth twice as much as schnitzel
            const APF_RATE: f64 = 5e8;
            const SCH_RATE: f64 = 1e9;
            const APFEL: &str = "APF";
            const SCHNITZEL: &str = "SCH";

            let conversion_table = {
                let mut t = HashMap::new();
                t.insert(
                    namada_core::types::address::apfel(),
                    args::BpConversionTableEntry {
                        alias: APFEL.into(),
                        conversion_rate: APF_RATE,
                    },
                );
                t.insert(
                    namada_core::types::address::schnitzel(),
                    args::BpConversionTableEntry {
                        alias: SCHNITZEL.into(),
                        conversion_rate: SCH_RATE,
                    },
                );
                t
            };

            let eligible = test_generate_eligible_aux(|ctx| {
                ctx.conversion_table.clone_from(&conversion_table);
                // tune the pending transfer provided by the ctx
                let transfer_paid_in_apfel = {
                    let mut pending = ctx.pending.clone();
                    pending.transfer.amount = 1.into();
                    pending.gas_fee.token =
                        namada_core::types::address::apfel();
                    pending
                };
                let transfer_paid_in_schnitzel = {
                    let mut pending = ctx.pending.clone();
                    pending.transfer.amount = 2.into();
                    pending.gas_fee.token =
                        namada_core::types::address::schnitzel();
                    pending
                };
                // add the transfers to the pool, and expect them to
                // be eligible transfers
                for (pending, rate) in [
                    (transfer_paid_in_apfel, APF_RATE),
                    (transfer_paid_in_schnitzel, SCH_RATE),
                ] {
                    ctx.signed_pool.insert(
                        pending.keccak256().to_string(),
                        pending.clone(),
                    );
                    ctx.expected_eligible.push(EligibleRecommendation {
                        transfer_hash: pending.keccak256().to_string(),
                        cost: transfer_fee()
                            - I256::from((1e9 / rate).floor() as u64)
                                * I256::try_from(pending.gas_fee.amount)
                                    .expect("Test failed"),
                        pending_transfer: pending,
                    });
                }
            });

            const VALIDATOR_GAS_FEE: Uint = Uint::from_u64(100_000);

            let recommended_batch = generate_recommendations::<DefaultIo>(
                eligible,
                &conversion_table,
                // gas spent by validator signature checks
                VALIDATOR_GAS_FEE,
                // unlimited amount of gas
                uint::MAX_VALUE,
                // only profitable
                I256::zero(),
            )
            .unwrap()
            .expect("Test failed");

            assert_eq!(
                recommended_batch.net_profit,
                I256::from(1_000_000_000_u64) + I256::from(2_000_000_000_u64)
                    - I256(VALIDATOR_GAS_FEE)
                    - transfer_fee() * I256::from(2_u64)
            );
        }
    }
}

pub use recommendations::recommend_batch;
