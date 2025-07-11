use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use std::collections::HashMap;

// Required for the subscription attribute below
use reth_chain_state as _;

/// Reth API namespace for reth-specific methods
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "reth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "reth"))]
pub trait RethApi {
    /// Returns all ETH balance changes in a block
    #[method(name = "getBalanceChangesInBlock")]
    async fn reth_get_balance_changes_in_block(
        &self,
        block_id: BlockId,
    ) -> RpcResult<HashMap<Address, U256>>;

    /// Subscribe to json `ChainNotifications`
    #[subscription(
        name = "subscribeChainNotifications",
        unsubscribe = "unsubscribeChainNotifications",
        item = reth_chain_state::CanonStateNotification
    )]
    async fn reth_subscribe_chain_notifications(&self) -> jsonrpsee::core::SubscriptionResult;
}
