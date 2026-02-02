use std::{ops::Deref as _, sync::Arc};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_rpc_types_eth::{request, TransactionRequest};
use eyre::Ok;
use futures::future::join_all;
use op_alloy_consensus::{OpTxEnvelope, TxDeposit};
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::{NamedChain, DEV};
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
use reth_node_core::args::DevArgs;
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
use reth_rpc::eth::DevSigner;
use reth_rpc_api::eth::helpers::{signer, EthSigner};
use reth_tasks::TaskManager;

// use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::OpTransactionRequest;

use alloy_network::eip2718::Encodable2718;
use alloy_primitives::{Address, Signature, TxKind, U256};
use reth_tracing::{
    tracing,
    tracing_subscriber::{self, EnvFilter},
};
use reth_transaction_pool::{
    identifier::{SenderId, TransactionId},
    TransactionOrigin, ValidPoolTransaction,
};
use reth_trie_db::ChangesetCache;
use tokio::time::Instant;

async fn generate_op_tx(idx: u64) -> OpPooledTransaction {
    let ddd = &DevArgs::default().dev_mnemonic;
    let signer = //: Box<dyn EthSigner<_, OpTransactionRequest>> =
        DevSigner::from_mnemonic::<_, OpTransactionRequest>(ddd, 1);
    let addr = *<dyn EthSigner<op_alloy_consensus::OpTxEnvelope, _> as EthSigner<
        op_alloy_consensus::OpTxEnvelope,
        OpTransactionRequest,
    >>::accounts(&*signer[0])
    .get(0)
    .unwrap();

    let chain_spec: OpChainSpec = OpChainSpecBuilder::optimism_mainnet().build();
    let c = chain_spec.chain().id();

    let tx = TxEip1559 {
        chain_id: c.into(),
        nonce: idx,
        max_fee_per_gas: 1000,
        max_priority_fee_per_gas: 0,
        gas_limit: 50000,
        to: Address::left_padding_from(&[6]).into(),
        value: U256::from(7_u64),
        input: vec![8].into(),
        access_list: Default::default(),
    };
    let sig = Signature::test_signature();

    let out = tx.encoded_for_signing();
    let sig = signer[0].sign(addr, &out).await.unwrap();
    let tx_signed = tx.into_signed(sig);
    let signed_tx: OpTransactionSigned = tx_signed.into();

    let signed_tx: OpTransactionSigned = signed_tx.into();
    let signed_recovered = Recovered::new_unchecked(signed_tx, addr);
    let len = 42; // causes compiler bug in nightly: signed_recovered.encode_2718_len();
    let pooled_tx: OpPooledTransaction = OpPooledTransaction::new(signed_recovered, len);
    pooled_tx
}

#[tokio::test]
async fn mock_payload_builder() -> eyre::Result<()> {
    // tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    reth_tracing::init_test_tracing();

    //let chain_spec = OpChainSpecBuilder::optimism_mainnet().build();
    // let mut chain_spec = **DEV;
    // not sure if this could work
    //chain_spec.chain = NamedChain::Optimism.into();
    let chain_spec = OpChainSpec::new((**DEV).clone());

    let da_config = OpDAConfig::new(430, 420);
    let evm_config = OpEvmConfig::optimism(OP_MAINNET.clone());

    let gas_limit_config = OpGasLimitConfig::new(4200000);
    dbg!(&gas_limit_config);

    let header = alloy_consensus::Header::default();
    let mut opba = OpPayloadBuilderAttributes::default();
    opba.gas_limit = gas_limit_config.gas_limit();
    let a = Arc::new(SealedHeader::new_unhashed(header));
    let config = PayloadConfig::new(a, opba);

    let pb = OpPayloadBuilderCtx {
        evm_config,
        builder_config: OpBuilderConfig::new(da_config, gas_limit_config),
        chain_spec: Arc::new(chain_spec),
        config,
        cancel: CancelOnDrop::default(),
        best_payload: None,
    };
    let mut db = reth_revm::State::builder().build();

    let mut info = ExecutionInfo::new();
    let mut mock_builder = pb.block_builder(&mut db).unwrap();
    let cc = (0..42).map(async |idx| {
        let vptx = ValidPoolTransaction {
            transaction: generate_op_tx(idx).await,
            transaction_id: TransactionId::new(SenderId::from(idx), 42),
            propagate: false,
            timestamp: Instant::now().into(),
            origin: TransactionOrigin::Private,
            authority_ids: None,
        };

        Arc::new(vptx)
    });

    let ccc = join_all(cc).await;

    let bb = BestPayloadTransactions::new(ccc.into_iter());

    dbg!(&bb);
    pb.execute_best_transactions(&mut info, &mut mock_builder, bb).unwrap();
    dbg!(&info);

    assert!(info.cumulative_da_bytes_used > 0);
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
