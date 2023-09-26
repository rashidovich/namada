use std::sync::Arc;

use color_eyre::eyre::Result;
use namada::eth_bridge::ethers::providers::{Http, Provider};
use namada::sdk::eth_bridge::{bridge_pool, validator_set};
use namada::types::io::Io;

use crate::cli;
use crate::cli::api::{CliApi, CliClient};
use crate::cli::args::{CliToSdk, CliToSdkCtxless};
use crate::cli::cmds::*;

impl<IO: Io> CliApi<IO> {
    pub async fn handle_relayer_command<C>(
        client: Option<C>,
        cmd: cli::NamadaRelayer,
    ) -> Result<()>
    where
        C: CliClient,
    {
        match cmd {
            cli::NamadaRelayer::EthBridgePoolWithCtx(boxed) => {
                let (sub, mut ctx) = *boxed;
                match sub {
                    EthBridgePoolWithCtx::RecommendBatch(RecommendBatch(
                        mut args,
                    )) => {
                        let client = client.unwrap_or_else(|| {
                            C::from_tendermint_address(
                                &mut args.query.ledger_address,
                            )
                        });
                        client.wait_until_node_is_synced::<IO>().await?;
                        let args = args.to_sdk(&mut ctx);
                        bridge_pool::recommend_batch::<_, IO>(&client, args)
                            .await?;
                    }
                }
            }
            cli::NamadaRelayer::EthBridgePoolWithoutCtx(sub) => match sub {
                EthBridgePoolWithoutCtx::ConstructProof(ConstructProof(
                    mut args,
                )) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(
                            &mut args.query.ledger_address,
                        )
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    let args = args.to_sdk_ctxless();
                    bridge_pool::construct_proof::<_, IO>(&client, args)
                        .await?;
                }
                EthBridgePoolWithoutCtx::RelayProof(RelayProof(mut args)) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(
                            &mut args.query.ledger_address,
                        )
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    let eth_client = Arc::new(
                        Provider::<Http>::try_from(&args.eth_rpc_endpoint)
                            .unwrap(),
                    );
                    let args = args.to_sdk_ctxless();
                    bridge_pool::relay_bridge_pool_proof::<_, _, IO>(
                        eth_client, &client, args,
                    )
                    .await?;
                }
                EthBridgePoolWithoutCtx::QueryPool(QueryEthBridgePool(
                    mut query,
                )) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(&mut query.ledger_address)
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    bridge_pool::query_bridge_pool::<_, IO>(&client).await?;
                }
                EthBridgePoolWithoutCtx::QuerySigned(
                    QuerySignedBridgePool(mut query),
                ) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(&mut query.ledger_address)
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    bridge_pool::query_signed_bridge_pool::<_, IO>(&client)
                        .await?;
                }
                EthBridgePoolWithoutCtx::QueryRelays(QueryRelayProgress(
                    mut query,
                )) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(&mut query.ledger_address)
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    bridge_pool::query_relay_progress::<_, IO>(&client).await?;
                }
            },
            cli::NamadaRelayer::ValidatorSet(sub) => match sub {
                ValidatorSet::BridgeValidatorSet(BridgeValidatorSet(
                    mut args,
                )) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(
                            &mut args.query.ledger_address,
                        )
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    let args = args.to_sdk_ctxless();
                    validator_set::query_bridge_validator_set::<_, IO>(
                        &client, args,
                    )
                    .await?;
                }
                ValidatorSet::GovernanceValidatorSet(
                    GovernanceValidatorSet(mut args),
                ) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(
                            &mut args.query.ledger_address,
                        )
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    let args = args.to_sdk_ctxless();
                    validator_set::query_governnace_validator_set::<_, IO>(
                        &client, args,
                    )
                    .await?;
                }
                ValidatorSet::ValidatorSetProof(ValidatorSetProof(
                    mut args,
                )) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(
                            &mut args.query.ledger_address,
                        )
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    let args = args.to_sdk_ctxless();
                    validator_set::query_validator_set_update_proof::<_, IO>(
                        &client, args,
                    )
                    .await?;
                }
                ValidatorSet::ValidatorSetUpdateRelay(
                    ValidatorSetUpdateRelay(mut args),
                ) => {
                    let client = client.unwrap_or_else(|| {
                        C::from_tendermint_address(
                            &mut args.query.ledger_address,
                        )
                    });
                    client.wait_until_node_is_synced::<IO>().await?;
                    let eth_client = Arc::new(
                        Provider::<Http>::try_from(&args.eth_rpc_endpoint)
                            .unwrap(),
                    );
                    let args = args.to_sdk_ctxless();
                    validator_set::relay_validator_set_update::<_, _, IO>(
                        eth_client, &client, args,
                    )
                    .await?;
                }
            },
        }
        Ok(())
    }
}
