use std::sync::Arc;

use eyre::Ok;
use op_alloy_consensus::TxDeposit;
use reth_basic_payload_builder::PayloadConfig;
use reth_db::{
    test_utils::{create_test_rw_db_with_path, TempDatabase},
    DatabaseEnv,
};
use reth_node_api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter};
use reth_node_builder::{
    common::{Attached, LaunchContextWith, WithComponents, WithConfigs},
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    hooks::OnComponentInitializedHook,
    LaunchContext, NodeConfig, RethFullAdapter,
};
use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder, OP_MAINNET, OP_SEPOLIA};
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::{
    node::OpPayloadBuilder, OpConsensusBuilder, OpDAConfig, OpExecutorBuilder, OpNetworkBuilder,
    OpNode, OpPayloadBuilderAttributes, OpPoolBuilder,
};
use reth_optimism_payload_builder::{
    builder::{self, ExecutionInfo, OpPayloadBuilderCtx},
    config::{OpBuilderConfig, OpGasLimitConfig},
};
use reth_optimism_primitives::OpTransactionSigned;
use reth_optimism_txpool::OpPooledTransaction;
use reth_payload_util::BestPayloadTransactions;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_provider::providers::BlockchainProvider;
use reth_revm::cancelled::CancelOnDrop;
use reth_tasks::TaskManager;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{TxKind, U256};
use reth_transaction_pool::{
    identifier::{SenderId, TransactionId},
    TransactionOrigin, ValidPoolTransaction,
};
use reth_trie_db::ChangesetCache;
use tokio::time::Instant;

fn generate_op_tx() -> OpPooledTransaction {
    let signer = Default::default();
    let deposit_tx = TxDeposit {
        source_hash: Default::default(),
        from: signer,
        to: TxKind::Create,
        mint: 0,
        value: U256::ZERO,
        gas_limit: 0,
        is_system_transaction: false,
        input: Default::default(),
    };
    let signed_tx: OpTransactionSigned = deposit_tx.into();
    let signed_recovered = Recovered::new_unchecked(signed_tx, signer);
    let len = 42; //signed_recovered.encode_2718_len();
    let pooled_tx: OpPooledTransaction = OpPooledTransaction::new(signed_recovered, len);
    pooled_tx
}

#[tokio::test]
async fn mock_payload_builder() -> eyre::Result<()> {
    let da_config = OpDAConfig::new(430, 420);
    let evm_config = OpEvmConfig::optimism(OP_MAINNET.clone());

    let gas_limit_config = OpGasLimitConfig::default();

    let header = alloy_consensus::Header::default();
    let opba = OpPayloadBuilderAttributes::default();
    let a = Arc::new(SealedHeader::new_unhashed(header));
    let config = PayloadConfig::new(a, opba);

    let pb = OpPayloadBuilderCtx {
        evm_config,
        builder_config: OpBuilderConfig::new(da_config, gas_limit_config),
        chain_spec: Arc::new(OpChainSpecBuilder::optimism_mainnet().build()),
        config,
        cancel: CancelOnDrop::default(),
        best_payload: None,
    };
    let mut db = reth_revm::State::builder().build();

    let mut info = ExecutionInfo::new();
    let mut mock_builder = pb.block_builder(&mut db).unwrap();
    let optx = generate_op_tx();
    let cc = (1..42).map(|idx| {
        let vptx = ValidPoolTransaction {
            transaction: optx.clone(),
            transaction_id: TransactionId::new(SenderId::from(idx), 42),
            propagate: false,
            timestamp: Instant::now().into(),
            origin: TransactionOrigin::Private,
            authority_ids: None,
        };

        Arc::new(vptx)
    });

    let bb = BestPayloadTransactions::new(cc);

    pb.execute_best_transactions(&mut info, &mut mock_builder, bb).unwrap();
    //dbg!(info);

    Ok(())
}

#[tokio::test]
#[ignore]
async fn mock_payload() -> eyre::Result<()> {
    let op_payload_builder = OpPayloadBuilder {
        compute_pending_block: false,
        best_transactions: (),
        da_config: OpDAConfig::default(),
        gas_limit_config: OpGasLimitConfig::default(),
    };

    // build core node with all components disabled except EVM and state
    let sepolia = NodeConfig::new(OP_SEPOLIA.clone());
    let db = create_test_rw_db_with_path(sepolia.datadir());
    let tasks = TaskManager::current();
    let launch_ctx = LaunchContext::new(tasks.executor(), sepolia.datadir());
    let node: reth_node_builder::common::LaunchContextWith<
        reth_node_builder::common::Attached<
            WithConfigs<OpChainSpec>,
            reth_node_builder::common::WithComponents<
                reth_node_api::FullNodeTypesAdapter<
                    OpNode,
                    Arc<Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>,
                    BlockchainProvider<
                        reth_node_api::NodeTypesWithDBAdapter<
                            OpNode,
                            Arc<Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>,
                        >,
                    >,
                >,
                ComponentsBuilder<
                    reth_node_api::FullNodeTypesAdapter<
                        OpNode,
                        Arc<Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>,
                        BlockchainProvider<
                            reth_node_api::NodeTypesWithDBAdapter<
                                OpNode,
                                Arc<Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>,
                            >,
                        >,
                    >,
                    OpPoolBuilder,
                    BasicPayloadServiceBuilder<OpPayloadBuilder>,
                    reth_optimism_node::OpNetworkBuilder,
                    OpExecutorBuilder,
                    reth_optimism_node::OpConsensusBuilder,
                >,
            >,
        >,
    > = launch_ctx
        .clone()
        .with_loaded_toml_config(sepolia.clone())
        .unwrap()
        .attach(Arc::new(db.clone()))
        .with_provider_factory::<_, OpEvmConfig>(ChangesetCache::new())
        .await
        .unwrap()
        .with_genesis()
        .unwrap()
        .with_metrics_task() // todo: shouldn't be req to set up blockchain db
        .with_blockchain_db::<RethFullAdapter<_, OpNode>, _>(move |provider_factory| {
            Ok(BlockchainProvider::new(provider_factory).unwrap())
        })
        .unwrap()
        .with_components(
            OpNode::default().components(),
            // ComponentsBuilder::default()
            // .node_types::<RethFullAdapter<_, OpNode>>()
            // .noop_pool::<OpPooledTransaction>()
            // .executor(OpExecutorBuilder::default()) .noop_consensus()
            // .noop_network::<OpNetworkPrimitives>()
            // // .payload::<OpPayloadBuilder>(op_payload_builder),
            // .noop_payload(),
            Box::new(()) as Box<dyn OnComponentInitializedHook<_>>,
        )
        .await
        .unwrap();

    let node2: LaunchContextWith<
        Attached<
            WithConfigs<OpChainSpec>,
            WithComponents<
                FullNodeTypesAdapter<
                    OpNode,
                    Arc<Arc<TempDatabase<DatabaseEnv>>>,
                    BlockchainProvider<
                        NodeTypesWithDBAdapter<OpNode, Arc<Arc<TempDatabase<DatabaseEnv>>>>,
                    >,
                >,
                ComponentsBuilder<
                    FullNodeTypesAdapter<
                        OpNode,
                        Arc<Arc<TempDatabase<DatabaseEnv>>>,
                        BlockchainProvider<
                            NodeTypesWithDBAdapter<OpNode, Arc<Arc<TempDatabase<DatabaseEnv>>>>,
                        >,
                    >,
                    OpPoolBuilder,
                    BasicPayloadServiceBuilder<OpPayloadBuilder>,
                    OpNetworkBuilder,
                    OpExecutorBuilder,
                    OpConsensusBuilder,
                >,
            >,
        >,
    > = launch_ctx
        .with_loaded_toml_config(sepolia)
        .unwrap()
        .attach(Arc::new(db))
        .with_provider_factory::<_, OpEvmConfig>(ChangesetCache::new())
        .await
        .unwrap()
        .with_genesis()
        .unwrap()
        .with_metrics_task() // todo: shouldn't be req to set up blockchain db
        .with_blockchain_db::<RethFullAdapter<_, OpNode>, _>(move |provider_factory| {
            Ok(BlockchainProvider::new(provider_factory).unwrap())
        })
        .unwrap()
        .with_components(
            OpNode::default().components(),
            // ComponentsBuilder::default()
            // .node_types::<RethFullAdapter<_, OpNode>>()
            // .noop_pool::<OpPooledTransaction>()
            // .executor(OpExecutorBuilder::default()) .noop_consensus()
            // .noop_network::<OpNetworkPrimitives>()
            // // .payload::<OpPayloadBuilder>(op_payload_builder),
            // .noop_payload(),
            Box::new(()) as Box<dyn OnComponentInitializedHook<_>>,
        )
        .await
        .unwrap();

    let b = OpPayloadBuilder::new(false)
        .with_da_config(OpDAConfig::default())
        .with_gas_limit_config(OpGasLimitConfig::default());

    // b.build_payload_builder(ctx, pool, evm_config);

    let a = BasicPayloadServiceBuilder::new(b);

    // node.components().

    Ok(())
}
