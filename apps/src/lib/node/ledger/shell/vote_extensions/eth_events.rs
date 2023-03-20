//! Extend Tendermint votes with Ethereum events seen by a quorum of validators.

use std::collections::{BTreeMap, HashMap};

use namada::ledger::pos::PosQueries;
use namada::ledger::storage::traits::StorageHasher;
use namada::ledger::storage::{DBIter, DB};
use namada::proto::Signed;
use namada::types::ethereum_events::EthereumEvent;
use namada::types::storage::BlockHeight;
use namada::types::token;
use namada::types::vote_extensions::ethereum_events::{
    self, MultiSignedEthEvent,
};
#[cfg(feature = "abcipp")]
use namada::types::voting_power::FractionalVotingPower;

use super::*;
use crate::node::ledger::shell::{Shell, ShellMode};

impl<D, H> Shell<D, H>
where
    D: DB + for<'iter> DBIter<'iter> + Sync + 'static,
    H: StorageHasher + Sync + 'static,
{
    /// Validates an Ethereum events vote extension issued at the provided
    /// block height.
    ///
    /// Checks that at epoch of the provided height:
    ///  * The Tendermint address corresponds to an active validator.
    ///  * The validator correctly signed the extension.
    ///  * The validator signed over the correct height inside of the extension.
    ///  * There are no duplicate Ethereum events in this vote extension, and
    ///    the events are sorted in ascending order.
    #[inline]
    #[allow(dead_code)]
    pub fn validate_eth_events_vext(
        &self,
        ext: Signed<ethereum_events::Vext>,
        last_height: BlockHeight,
    ) -> bool {
        self.validate_eth_events_vext_and_get_it_back(ext, last_height)
            .is_ok()
    }

    /// This method behaves exactly like [`Self::validate_eth_events_vext`],
    /// with the added bonus of returning the vote extension back, if it
    /// is valid.
    pub fn validate_eth_events_vext_and_get_it_back(
        &self,
        ext: Signed<ethereum_events::Vext>,
        last_height: BlockHeight,
    ) -> std::result::Result<
        (token::Amount, Signed<ethereum_events::Vext>),
        VoteExtensionError,
    > {
        // NOTE(not(feature = "abciplus")): for ABCI++, we should pass
        // `last_height` here, instead of `ext.data.block_height`
        let ext_height_epoch = match self
            .wl_storage
            .pos_queries()
            .get_epoch(ext.data.block_height)
        {
            Some(epoch) => epoch,
            _ => {
                tracing::debug!(
                    block_height = ?ext.data.block_height,
                    "The epoch of the Ethereum events vote extension's \
                     block height should always be known",
                );
                return Err(VoteExtensionError::UnexpectedEpoch);
            }
        };
        if !self
            .wl_storage
            .ethbridge_queries()
            .is_bridge_active_at(ext_height_epoch)
        {
            tracing::debug!(
                vext_epoch = ?ext_height_epoch,
                "The Ethereum bridge was not enabled when the Ethereum
                 events' vote extension was cast",
            );
            return Err(VoteExtensionError::EthereumBridgeInactive);
        }
        #[cfg(feature = "abcipp")]
        if ext.data.block_height != last_height {
            tracing::debug!(
                ext_height = ?ext.data.block_height,
                ?last_height,
                "Ethereum events vote extension issued for a block height \
                 different from the expected last height."
            );
            return Err(VoteExtensionError::UnexpectedBlockHeight);
        }
        #[cfg(not(feature = "abcipp"))]
        if ext.data.block_height > last_height {
            tracing::debug!(
                ext_height = ?ext.data.block_height,
                ?last_height,
                "Ethereum events vote extension issued for a block height \
                 higher than the chain's last height."
            );
            return Err(VoteExtensionError::UnexpectedBlockHeight);
        }
        if ext.data.block_height.0 == 0 {
            tracing::debug!("Dropping vote extension issued at genesis");
            return Err(VoteExtensionError::UnexpectedBlockHeight);
        }
        // verify if we have any duplicate Ethereum events,
        // and if these are sorted in ascending order
        let have_dupes_or_non_sorted = {
            !ext.data
                .ethereum_events
                // TODO: move to `array_windows` when it reaches Rust stable
                .windows(2)
                .all(|evs| evs[0] < evs[1])
        };
        let validator = &ext.data.validator_addr;
        if have_dupes_or_non_sorted {
            tracing::debug!(
                %validator,
                "Found duplicate or non-sorted Ethereum events in a vote extension from \
                 some validator"
            );
            return Err(VoteExtensionError::HaveDupesOrNonSorted);
        }
        // get the public key associated with this validator
        let (voting_power, pk) = self
            .wl_storage
            .pos_queries()
            .get_validator_from_address(validator, Some(ext_height_epoch))
            .map_err(|err| {
                tracing::debug!(
                    ?err,
                    %validator,
                    "Could not get public key from Storage for some validator, \
                     while validating Ethereum events vote extension"
                );
                VoteExtensionError::PubKeyNotInStorage
            })?;
        // verify the signature of the vote extension
        ext.verify(&pk)
            .map_err(|err| {
                tracing::debug!(
                    ?err,
                    ?ext.sig,
                    ?pk,
                    %validator,
                    "Failed to verify the signature of an Ethereum events vote \
                     extension issued by some validator"
                );
                VoteExtensionError::VerifySigFailed
            })
            .map(|_| (voting_power, ext))
    }

    /// Checks the channel from the Ethereum oracle monitoring
    /// the fullnode and retrieves all seen Ethereum events.
    pub fn new_ethereum_events(&mut self) -> Vec<EthereumEvent> {
        match &mut self.mode {
            ShellMode::Validator {
                eth_oracle:
                    Some(EthereumOracleChannels {
                        ethereum_receiver, ..
                    }),
                ..
            } => {
                ethereum_receiver.fill_queue();
                ethereum_receiver.get_events()
            }
            _ => vec![],
        }
    }

    /// Takes an iterator over Ethereum events vote extension instances,
    /// and returns another iterator. The latter yields
    /// valid Ethereum events vote extensions, or the reason why these
    /// are invalid, in the form of a [`VoteExtensionError`].
    #[inline]
    pub fn validate_eth_events_vext_list<'iter>(
        &'iter self,
        vote_extensions: impl IntoIterator<Item = Signed<ethereum_events::Vext>>
        + 'iter,
    ) -> impl Iterator<
        Item = std::result::Result<
            (token::Amount, Signed<ethereum_events::Vext>),
            VoteExtensionError,
        >,
    > + 'iter {
        vote_extensions.into_iter().map(|vote_extension| {
            self.validate_eth_events_vext_and_get_it_back(
                vote_extension,
                self.wl_storage.storage.last_height,
            )
        })
    }

    /// Takes a list of signed Ethereum events vote extensions,
    /// and filters out invalid instances.
    #[inline]
    pub fn filter_invalid_eth_events_vexts<'iter>(
        &'iter self,
        vote_extensions: impl IntoIterator<Item = Signed<ethereum_events::Vext>>
        + 'iter,
    ) -> impl Iterator<Item = (token::Amount, Signed<ethereum_events::Vext>)> + 'iter
    {
        self.validate_eth_events_vext_list(vote_extensions)
            .filter_map(|ext| ext.ok())
    }

    /// Compresses a set of signed Ethereum events into a single
    /// [`ethereum_events::VextDigest`], whilst filtering invalid
    /// [`Signed<ethereum_events::Vext>`] instances in the process.
    ///
    /// When vote extensions are being used, this performs a check
    /// that at least 2/3 of the validators by voting power have
    /// included ethereum events in their vote extension.
    pub fn compress_ethereum_events(
        &self,
        vote_extensions: Vec<Signed<ethereum_events::Vext>>,
    ) -> Option<ethereum_events::VextDigest> {
        #[cfg(not(feature = "abcipp"))]
        if self.wl_storage.storage.last_height == BlockHeight(0) {
            return None;
        }

        #[cfg(feature = "abcipp")]
        let vexts_epoch = self
            .wl_storage
            .pos_queries()
            .get_epoch(self.wl_storage.storage.last_height)
            .expect(
                "The epoch of the last block height should always be known",
            );

        #[cfg(feature = "abcipp")]
        let total_voting_power = u64::from(
            self.wl_storage
                .pos_queries()
                .get_total_voting_power(Some(vexts_epoch)),
        );
        #[cfg(feature = "abcipp")]
        let mut voting_power = FractionalVotingPower::default();

        let mut event_observers = BTreeMap::new();
        let mut signatures = HashMap::new();

        for (_validator_voting_power, vote_extension) in
            self.filter_invalid_eth_events_vexts(vote_extensions)
        {
            let validator_addr = vote_extension.data.validator_addr;
            let block_height = vote_extension.data.block_height;

            // update voting power
            #[cfg(feature = "abcipp")]
            {
                let validator_voting_power = u64::from(_validator_voting_power);
                voting_power += FractionalVotingPower::new(
                    validator_voting_power,
                    total_voting_power,
                )
                .expect(
                    "The voting power we obtain from storage should always be \
                     valid",
                );
            }

            // register all ethereum events seen by `validator_addr`
            for ev in vote_extension.data.ethereum_events {
                let signers =
                    event_observers.entry(ev).or_insert_with(BTreeSet::new);
                signers.insert((validator_addr.clone(), block_height));
            }

            // register the signature of `validator_addr`
            let addr = validator_addr.clone();
            let sig = vote_extension.sig;

            let key = (addr, block_height);
            tracing::debug!(
                ?key,
                ?sig,
                ?validator_addr,
                "Inserting signature into ethereum_events::VextDigest"
            );
            if let Some(existing_sig) = signatures.insert(key, sig.clone()) {
                tracing::warn!(
                    ?sig,
                    ?existing_sig,
                    ?validator_addr,
                    "Overwrote old signature from validator while \
                     constructing ethereum_events::VextDigest - maybe private \
                     key of validator is being used by multiple nodes?"
                );
            }
        }

        #[cfg(feature = "abcipp")]
        if voting_power <= FractionalVotingPower::TWO_THIRDS {
            tracing::error!(
                "Tendermint has decided on a block including Ethereum events \
                 reflecting <= 2/3 of the total stake"
            );
            return None;
        }

        let events: Vec<MultiSignedEthEvent> = event_observers
            .into_iter()
            .map(|(event, signers)| MultiSignedEthEvent { event, signers })
            .collect();

        Some(ethereum_events::VextDigest { events, signatures })
    }
}

#[cfg(test)]
mod test_vote_extensions {
    use std::collections::HashSet;
    use std::convert::TryInto;

    #[cfg(feature = "abcipp")]
    use borsh::BorshDeserialize;
    use borsh::BorshSerialize;
    use namada::core::ledger::storage_api::collections::lazy_map::{
        NestedSubKey, SubKey,
    };
    use namada::core::types::erc20tokens::Erc20Amount;
    use namada::eth_bridge::oracle::config::UpdateErc20;
    use namada::eth_bridge::storage::wrapped_erc20s;
    use namada::ledger::pos::PosQueries;
    use namada::proof_of_stake::consensus_validator_set_handle;
    #[cfg(feature = "abcipp")]
    use namada::proto::{SignableEthMessage, Signed};
    use namada::types::address::testing::gen_established_address;
    use namada::types::address::wnam;
    #[cfg(feature = "abcipp")]
    use namada::types::eth_abi::Encode;
    #[cfg(feature = "abcipp")]
    use namada::types::ethereum_events::Uint;
    use namada::types::ethereum_events::{
        EthAddress, EthereumEvent, TokenWhitelist, TransferToEthereum,
    };
    #[cfg(feature = "abcipp")]
    use namada::types::keccak::keccak_hash;
    #[cfg(feature = "abcipp")]
    use namada::types::keccak::KeccakHash;
    use namada::types::key::*;
    use namada::types::storage::{BlockHeight, Epoch, TxIndex};
    use namada::types::token::Amount;
    use namada::types::transaction::protocol::{ProtocolTx, ProtocolTxType};
    use namada::types::transaction::TxType;
    #[cfg(feature = "abcipp")]
    use namada::types::vote_extensions::bridge_pool_roots;
    use namada::types::vote_extensions::ethereum_events;
    #[cfg(feature = "abcipp")]
    use namada::types::vote_extensions::VoteExtension;
    use namada::vm::wasm::{TxCache, VpCache};
    use namada::vm::WasmCacheRwAccess;
    use tempfile::tempdir;

    #[cfg(feature = "abcipp")]
    use crate::facade::tendermint_proto::abci::response_verify_vote_extension::VerifyStatus;
    #[cfg(feature = "abcipp")]
    use crate::facade::tower_abci::request;
    use crate::node::ledger::oracle::control::Command;
    use crate::node::ledger::shell::test_utils::*;
    use crate::node::ledger::shims::abcipp_shim_types::shim::request::FinalizeBlock;

    /// Test that we successfully receive ethereum events
    /// from the channel to fullnode process
    ///
    /// We further check that ledger side buffering is done if multiple
    /// events are in the channel and that queueing and de-duplicating is
    /// done
    #[test]
    fn test_get_eth_events() {
        let (mut shell, _, oracle, _) = setup();
        let event_1 = EthereumEvent::TransfersToEthereum {
            nonce: 1.into(),
            transfers: vec![TransferToEthereum {
                amount: Amount::from(100).into(),
                asset: EthAddress([1; 20]),
                sender: gen_established_address(),
                receiver: EthAddress([2; 20]),
                gas_amount: 10.into(),
                gas_payer: gen_established_address(),
            }],
            relayer: gen_established_address(),
        };
        let event_2 = EthereumEvent::TransfersToEthereum {
            nonce: 2.into(),
            transfers: vec![TransferToEthereum {
                amount: Amount::from(100).into(),
                asset: EthAddress([1; 20]),
                sender: gen_established_address(),
                receiver: EthAddress([2; 20]),
                gas_amount: 10.into(),
                gas_payer: gen_established_address(),
            }],
            relayer: gen_established_address(),
        };
        let event_3 = EthereumEvent::NewContract {
            name: "Test".to_string(),
            address: EthAddress([0; 20]),
        };

        tokio_test::block_on(oracle.send(event_1.clone()))
            .expect("Test failed");
        tokio_test::block_on(oracle.send(event_3.clone()))
            .expect("Test failed");
        let [event_first, event_second]: [EthereumEvent; 2] =
            shell.new_ethereum_events().try_into().expect("Test failed");

        assert_eq!(event_first, event_1);
        assert_eq!(event_second, event_3);
        // check that we queue and de-duplicate events
        tokio_test::block_on(oracle.send(event_2.clone()))
            .expect("Test failed");
        tokio_test::block_on(oracle.send(event_3.clone()))
            .expect("Test failed");
        let [event_first, event_second, event_third]: [EthereumEvent; 3] =
            shell.new_ethereum_events().try_into().expect("Test failed");

        assert_eq!(event_first, event_1);
        assert_eq!(event_second, event_2);
        assert_eq!(event_third, event_3);
    }

    /// Test that ethereum events are added to vote extensions.
    /// Check that vote extensions pass verification.
    #[cfg(feature = "abcipp")]
    #[tokio::test]
    async fn test_eth_events_vote_extension() {
        let (mut shell, _, oracle, _) = setup_at_height(1);
        let address = shell
            .mode
            .get_validator_address()
            .expect("Test failed")
            .clone();
        let event_1 = EthereumEvent::TransfersToEthereum {
            nonce: 1.into(),
            transfers: vec![TransferToEthereum {
                amount: 100.into(),
                asset: EthAddress([1; 20]),
                sender: gen_established_address(),
                receiver: EthAddress([2; 20]),
                gas_amount: 10.into(),
                gas_payer: gen_established_address(),
            }],
            relayer: gen_established_address(),
        };
        let event_2 = EthereumEvent::NewContract {
            name: "Test".to_string(),
            address: EthAddress([0; 20]),
        };
        oracle.send(event_1.clone()).await.expect("Test failed");
        oracle.send(event_2.clone()).await.expect("Test failed");
        let vote_extension =
            <VoteExtension as BorshDeserialize>::try_from_slice(
                &shell.extend_vote(Default::default()).vote_extension[..],
            )
            .expect("Test failed");

        let [event_first, event_second]: [EthereumEvent; 2] = vote_extension
            .ethereum_events
            .clone()
            .expect("Test failed")
            .data
            .ethereum_events
            .try_into()
            .expect("Test failed");

        assert_eq!(event_first, event_1);
        assert_eq!(event_second, event_2);
        let req = request::VerifyVoteExtension {
            hash: vec![],
            validator_address: address
                .raw_hash()
                .expect("Test failed")
                .as_bytes()
                .to_vec(),
            height: 1,
            vote_extension: vote_extension.try_to_vec().expect("Test failed"),
        };
        let res = shell.verify_vote_extension(req);
        assert_eq!(res.status, i32::from(VerifyStatus::Accept));
    }

    /// Test that Ethereum events signed by a non-validator are rejected
    #[test]
    fn test_eth_events_must_be_signed_by_validator() {
        let (shell, _, _, _) = setup_at_height(3u64);
        let signing_key = gen_keypair();
        let address = shell
            .mode
            .get_validator_address()
            .expect("Test failed")
            .clone();
        #[allow(clippy::redundant_clone)]
        let ethereum_events = ethereum_events::Vext {
            ethereum_events: vec![EthereumEvent::TransfersToEthereum {
                nonce: 1.into(),
                transfers: vec![TransferToEthereum {
                    amount: Amount::from(100).into(),
                    sender: gen_established_address(),
                    asset: EthAddress([1; 20]),
                    receiver: EthAddress([2; 20]),
                    gas_amount: 10.into(),
                    gas_payer: gen_established_address(),
                }],
                relayer: gen_established_address(),
            }],
            block_height: shell
                .wl_storage
                .pos_queries()
                .get_current_decision_height(),
            validator_addr: address.clone(),
        }
        .sign(&signing_key);
        #[cfg(feature = "abcipp")]
        let req = request::VerifyVoteExtension {
            hash: vec![],
            validator_address: address
                .raw_hash()
                .expect("Test failed")
                .as_bytes()
                .to_vec(),
            height: 0,
            vote_extension: VoteExtension {
                ethereum_events: Some(ethereum_events.clone()),
                bridge_pool_root: {
                    let to_sign = keccak_hash(
                        [
                            KeccakHash([0; 32]).encode().into_inner(),
                            Uint::from(0).encode().into_inner(),
                        ]
                        .concat(),
                    );
                    let sig = Signed::<_, SignableEthMessage>::new(
                        shell
                            .mode
                            .get_eth_bridge_keypair()
                            .expect("Test failed"),
                        to_sign,
                    )
                    .sig;
                    Some(
                        bridge_pool_roots::Vext {
                            block_height: shell.wl_storage.storage.last_height,
                            validator_addr: address,
                            sig,
                        }
                        .sign(
                            shell.mode.get_protocol_key().expect("Test failed"),
                        ),
                    )
                },
                validator_set_update: None,
            }
            .try_to_vec()
            .expect("Test failed"),
        };
        #[cfg(feature = "abcipp")]
        assert_eq!(
            shell.verify_vote_extension(req).status,
            i32::from(VerifyStatus::Reject)
        );
        assert!(!shell.validate_eth_events_vext(
            ethereum_events,
            shell.wl_storage.pos_queries().get_current_decision_height(),
        ))
    }

    /// Test that validation of Ethereum events cast during the
    /// previous block are accepted for the current block. This
    /// should pass even if the epoch changed resulting in a
    /// change to the validator set.
    #[test]
    fn test_validate_eth_events_vexts() {
        let (mut shell, _recv, _, _oracle_control_recv) = setup_at_height(3u64);
        let signing_key =
            shell.mode.get_protocol_key().expect("Test failed").clone();
        let address = shell
            .mode
            .get_validator_address()
            .expect("Test failed")
            .clone();
        let signed_height =
            shell.wl_storage.pos_queries().get_current_decision_height();
        let vote_ext = ethereum_events::Vext {
            ethereum_events: vec![EthereumEvent::TransfersToEthereum {
                nonce: 1.into(),
                transfers: vec![TransferToEthereum {
                    amount: Amount::from(100).into(),
                    sender: gen_established_address(),
                    asset: EthAddress([1; 20]),
                    receiver: EthAddress([2; 20]),
                    gas_amount: 10.into(),
                    gas_payer: gen_established_address(),
                }],
                relayer: gen_established_address(),
            }],
            block_height: signed_height,
            validator_addr: address,
        }
        .sign(shell.mode.get_protocol_key().expect("Test failed"));

        assert_eq!(shell.wl_storage.storage.get_current_epoch().0.0, 0);
        // remove all validators of the next epoch
        let validators_handle = consensus_validator_set_handle().at(&1.into());
        let consensus_in_mem = validators_handle
            .iter(&shell.wl_storage)
            .expect("Test failed")
            .map(|val| {
                let (
                    NestedSubKey::Data {
                        key: stake,
                        nested_sub_key: SubKey::Data(position),
                    },
                    ..,
                ) = val.expect("Test failed");
                (stake, position)
            })
            .collect::<Vec<_>>();
        for (val_stake, val_position) in consensus_in_mem.into_iter() {
            validators_handle
                .at(&val_stake)
                .remove(&mut shell.wl_storage, &val_position)
                .expect("Test failed");
        }
        // we advance forward to the next epoch
        let mut req = FinalizeBlock::default();
        req.header.time = namada::types::time::DateTimeUtc::now();
        shell.wl_storage.storage.last_height = BlockHeight(11);
        shell.finalize_block(req).expect("Test failed");
        shell.commit();
        assert_eq!(shell.wl_storage.storage.get_current_epoch().0.0, 1);
        assert!(
            shell
                .wl_storage
                .pos_queries()
                .get_validator_from_protocol_pk(&signing_key.ref_to(), None)
                .is_err()
        );
        let prev_epoch =
            Epoch(shell.wl_storage.storage.get_current_epoch().0.0 - 1);
        assert!(
            shell
                .shell
                .wl_storage
                .pos_queries()
                .get_validator_from_protocol_pk(
                    &signing_key.ref_to(),
                    Some(prev_epoch)
                )
                .is_ok()
        );

        assert!(shell.validate_eth_events_vext(vote_ext, signed_height));
    }

    /// Test that we correctly identify changes to ERC20 whitelist
    /// storage and forward that info the Ethereum oracle.
    #[tokio::test]
    async fn test_update_oracle() {
        let (mut shell, _, _, mut oracle_command) = setup();
        let Command::UpdateConfig(config) =
            oracle_command.recv().await.expect("Test failed");
        let expected = HashSet::from([UpdateErc20::Add(wnam(), 6)]);
        let actual: HashSet<UpdateErc20> =
            config.whitelist_update.into_iter().collect();
        assert_eq!(actual, expected);

        let address = shell
            .mode
            .get_validator_address()
            .expect("Test failed")
            .clone();
        let signed_height =
            shell.wl_storage.pos_queries().get_current_decision_height();
        let asset_1 = EthAddress([0; 20]);
        let asset_2 = EthAddress([1; 20]);
        let asset_3 = EthAddress([2; 20]);
        let keys_1 = wrapped_erc20s::Keys::from(&asset_1);
        let keys_2 = wrapped_erc20s::Keys::from(&asset_2);

        shell
            .wl_storage
            .storage
            .write(
                &keys_1.cap(),
                Erc20Amount::from_int(1000u64, 8)
                    .expect("Test failed")
                    .try_to_vec()
                    .expect("Test failed"),
            )
            .expect("Test failed");
        shell
            .wl_storage
            .storage
            .write(
                &keys_2.cap(),
                Erc20Amount::from_int(1000u64, 9)
                    .expect("Test failed")
                    .try_to_vec()
                    .expect("Test failed"),
            )
            .expect("Test failed");
        shell
            .wl_storage
            .storage
            .write(
                &keys_1.denomination(),
                8u8.try_to_vec().expect("Test failed"),
            )
            .expect("Test failed");
        shell
            .wl_storage
            .storage
            .write(
                &keys_2.denomination(),
                9u8.try_to_vec().expect("Test failed"),
            )
            .expect("Test failed");
        let vote_ext = ethereum_events::Vext {
            ethereum_events: vec![EthereumEvent::UpdateBridgeWhitelist {
                nonce: 1.into(),
                whitelist: vec![
                    TokenWhitelist {
                        token: asset_1,
                        cap: Erc20Amount::from_int(10_000u64, 8)
                            .expect("Test failed"),
                    },
                    TokenWhitelist {
                        token: asset_3,
                        cap: Erc20Amount::from_int(5000u64, 10)
                            .expect("Test failed"),
                    },
                ],
            }],
            block_height: signed_height,
            validator_addr: address,
        }
        .sign(shell.mode.get_protocol_key().expect("Test failed"));

        let mut gas_meter = Default::default();
        let mut vp_wasm_cache = VpCache::<WasmCacheRwAccess>::new(
            tempdir().unwrap().as_ref().canonicalize().unwrap(),
            10,
        );
        let mut tx_wasm_cache = TxCache::<WasmCacheRwAccess>::new(
            tempdir().unwrap().as_ref().canonicalize().unwrap(),
            10,
        );
        namada::ledger::protocol::dispatch_tx(
            TxType::Protocol(ProtocolTx {
                pk: shell
                    .mode
                    .get_protocol_key()
                    .expect("Test failed")
                    .ref_to(),
                tx: ProtocolTxType::EthEventsVext(vote_ext),
            }),
            0,
            TxIndex(0),
            &mut gas_meter,
            &mut shell.wl_storage,
            &mut vp_wasm_cache,
            &mut tx_wasm_cache,
        )
        .expect("Test failed");
        shell.update_eth_oracle();
        let Command::UpdateConfig(config) =
            oracle_command.recv().await.expect("Test failed");
        let expected = HashSet::from([
            UpdateErc20::Add(asset_1, 8),
            UpdateErc20::Add(asset_3, 10),
            UpdateErc20::Add(wnam(), 6),
            UpdateErc20::Remove(asset_2),
        ]);
        let actual: HashSet<UpdateErc20> =
            config.whitelist_update.into_iter().collect();
        assert_eq!(actual, expected);
    }

    /// Test for ABCI++ that an [`ethereum_events::Vext`] that incorrectly
    /// labels what block it was included on in a vote extension is
    /// rejected. For ABCI+, test that it is rejected if the block height is
    /// greater than latest block height.
    #[test]
    fn reject_incorrect_block_number() {
        let (shell, _, _, _) = setup_at_height(3u64);
        let address = shell.mode.get_validator_address().unwrap().clone();
        #[allow(clippy::redundant_clone)]
        let mut ethereum_events = ethereum_events::Vext {
            ethereum_events: vec![EthereumEvent::TransfersToEthereum {
                nonce: 1.into(),
                transfers: vec![TransferToEthereum {
                    amount: Amount::from(100).into(),
                    sender: gen_established_address(),
                    asset: EthAddress([1; 20]),
                    receiver: EthAddress([2; 20]),
                    gas_amount: 10.into(),
                    gas_payer: gen_established_address(),
                }],
                relayer: gen_established_address(),
            }],
            block_height: shell.wl_storage.storage.last_height,
            validator_addr: address.clone(),
        };

        #[cfg(feature = "abcipp")]
        {
            let signed_vext = ethereum_events
                .clone()
                .sign(shell.mode.get_protocol_key().expect("Test failed"));
            let bp_root = {
                let to_sign = keccak_hash(
                    [
                        KeccakHash([0; 32]).encode().into_inner(),
                        Uint::from(0).encode().into_inner(),
                    ]
                    .concat(),
                );
                let sig = Signed::<_, SignableEthMessage>::new(
                    shell.mode.get_eth_bridge_keypair().expect("Test failed"),
                    to_sign,
                )
                .sig;
                bridge_pool_roots::Vext {
                    block_height: shell.wl_storage.storage.last_height,
                    validator_addr: address.clone(),
                    sig,
                }
                .sign(shell.mode.get_protocol_key().expect("Test failed"))
            };
            let req = request::VerifyVoteExtension {
                hash: vec![],
                validator_address: address.try_to_vec().expect("Test failed"),
                height: 0,
                vote_extension: VoteExtension {
                    ethereum_events: Some(signed_vext),
                    bridge_pool_root: Some(bp_root),
                    validator_set_update: None,
                }
                .try_to_vec()
                .expect("Test failed"),
            };

            assert_eq!(
                shell.verify_vote_extension(req).status,
                i32::from(VerifyStatus::Reject)
            );
        }

        ethereum_events.block_height = shell.wl_storage.storage.last_height + 1;
        let signed_vext = ethereum_events
            .sign(shell.mode.get_protocol_key().expect("Test failed"));
        assert!(!shell.validate_eth_events_vext(
            signed_vext,
            shell.wl_storage.storage.last_height
        ))
    }

    /// Test if we reject Ethereum events vote extensions
    /// issued at genesis
    #[test]
    fn test_reject_genesis_vexts() {
        let (shell, _, _, _) = setup();
        let address = shell.mode.get_validator_address().unwrap().clone();
        #[allow(clippy::redundant_clone)]
        let vote_ext = ethereum_events::Vext {
            ethereum_events: vec![EthereumEvent::TransfersToEthereum {
                nonce: 1.into(),
                transfers: vec![TransferToEthereum {
                    amount: Amount::from(100).into(),
                    sender: gen_established_address(),
                    asset: EthAddress([1; 20]),
                    receiver: EthAddress([2; 20]),
                    gas_amount: 10.into(),
                    gas_payer: gen_established_address(),
                }],
                relayer: gen_established_address(),
            }],
            block_height: shell.wl_storage.storage.last_height,
            validator_addr: address.clone(),
        }
        .sign(shell.mode.get_protocol_key().expect("Test failed"));

        #[cfg(feature = "abcipp")]
        let req = request::VerifyVoteExtension {
            hash: vec![],
            validator_address: address.try_to_vec().expect("Test failed"),
            height: 0,
            vote_extension: vote_ext.try_to_vec().expect("Test failed"),
        };
        #[cfg(feature = "abcipp")]
        assert_eq!(
            shell.verify_vote_extension(req).status,
            i32::from(VerifyStatus::Reject)
        );
        assert!(!shell.validate_eth_events_vext(
            vote_ext,
            shell.wl_storage.storage.last_height
        ))
    }
}