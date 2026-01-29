#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_op_evm::block::OpAlloyReceiptBuilder;
    use alloy_op_hardforks::op_sepolia;
    use alloy_rpc_types_eth::Header;
    use eyre::Ok;
    use reth_basic_payload_builder::PayloadConfig;
    use reth_chainspec::Head;
    use reth_db::{open_db_read_only, test_utils::create_test_rw_db_with_path};
    use reth_evm::{
        execute::{BlockBuilder, BlockExecutorFactory},
        noop::NoopEvmConfig,
        Evm, EvmEnv, EvmFactory,
    };
    use reth_evm_ethereum::MockExecutor;
    use reth_node_builder::{
        common::WithConfigs,
        components::{
            BasicPayloadServiceBuilder, ComponentsBuilder, PayloadBuilderBuilder,
            PayloadServiceBuilder, PoolBuilder,
        },
        hooks::OnComponentInitializedHook,
        BuilderContext, LaunchContext, Node, NodeComponents, NodeComponentsBuilder, NodeConfig,
        RethFullAdapter,
    };
    use reth_optimism_chainspec::{OpChainSpec, OpChainSpecBuilder, OP_MAINNET, OP_SEPOLIA};
    use reth_optimism_evm::{
        OpBlockExecutionCtx, OpBlockExecutorFactory, OpEvmConfig, OpEvmFactory,
    };
    use reth_optimism_node::{
        node::OpPayloadBuilder, OpDAConfig, OpExecutorBuilder, OpFullNodeTypes,
        OpNetworkPrimitives, OpNode, OpPayloadBuilderAttributes, OpPoolBuilder,
    };
    use reth_optimism_payload_builder::{
        builder::{self, ExecutionInfo, OpPayloadBuilderCtx, OpPayloadTransactions},
        config::{OpBuilderConfig, OpGasLimitConfig},
    };
    use reth_optimism_txpool::OpPooledTransaction;
    use reth_primitives_traits::SealedHeader;
    use reth_provider::providers::{BlockchainProvider, RocksDBProvider, StaticFileProvider};
    use reth_revm::{
        cancelled::CancelOnDrop,
        db::{CacheDB, EmptyDB},
        State,
    };
    use reth_tasks::{TaskExecutor, TaskManager, TokioTaskExecutor};
    use reth_trie_db::ChangesetCache;

    use alloy_primitives::Address;
    use proptest::{
        arbitrary::Arbitrary, prelude::*, strategy::ValueTree, test_runner::TestRunner,
    };
    use reth_transaction_pool::{
        pool::{BasefeeOrd, BlobTransactions, ParkedPool, PendingPool, QueuedOrd},
        test_utils::{MockOrdering, MockTransaction, MockTransactionFactory},
        SubPoolLimit,
    };

    /// Generates a set of `depth` dependent transactions, with the specified sender. Its values are
    /// generated using [Arbitrary].
    fn create_transactions_for_sender(
        runner: &mut TestRunner,
        sender: Address,
        depth: usize,
        only_eip4844: bool,
    ) -> Vec<MockTransaction> {
        // assert that depth is always greater than zero, since empty vecs do not really make sense
        // in this context
        assert!(depth > 0);

        if only_eip4844 {
            return prop::collection::vec(
                any::<MockTransaction>().prop_filter("only eip4844", |tx| tx.is_eip4844()),
                depth,
            )
            .new_tree(runner)
            .unwrap()
            .current();
        }

        // make sure these are all post-eip-1559 transactions
        let mut txs = prop::collection::vec(any::<MockTransaction>(), depth)
            .new_tree(runner)
            .unwrap()
            .current();

        for (nonce, tx) in txs.iter_mut().enumerate() {
            // reject pre-eip1559 tx types, if there is a legacy tx, replace it with an eip1559 tx
            if tx.is_legacy() || tx.is_eip2930() {
                *tx = MockTransaction::eip1559();

                // set fee values using arbitrary
                tx.set_priority_fee(any::<u128>().new_tree(runner).unwrap().current());
                tx.set_max_fee(any::<u128>().new_tree(runner).unwrap().current());
            }

            tx.set_sender(sender);
            tx.set_nonce(nonce as u64);
        }

        txs
    }

    /// Generates many transactions, each with a different sender. The number of transactions per
    /// sender is generated using [Arbitrary]. The number of senders is specified by `senders`.
    ///
    /// Because this uses [Arbitrary], the number of transactions per sender needs to be bounded.
    /// This is done by using the `max_depth` parameter.
    ///
    /// This uses [`create_transactions_for_sender`] to generate the transactions.
    fn generate_many_transactions(
        senders: usize,
        max_depth: usize,
        only_eip4844: bool,
    ) -> Vec<MockTransaction> {
        let mut runner = TestRunner::deterministic();

        let mut txs = Vec::with_capacity(senders);
        for idx in 0..senders {
            // modulo max_depth so we know it is bounded, plus one so the minimum is always 1
            let depth = any::<usize>().new_tree(&mut runner).unwrap().current() % max_depth + 1;

            // set sender to an Address determined by the sender index. This should make any
            // necessary debugging easier.
            let idx_slice = idx.to_be_bytes();

            // pad with 12 bytes of zeros before rest
            let addr_slice = [0u8; 12].into_iter().chain(idx_slice.into_iter()).collect::<Vec<_>>();

            let sender = Address::from_slice(&addr_slice);
            txs.extend(create_transactions_for_sender(&mut runner, sender, depth, only_eip4844));
        }

        txs
    }
    #[tokio::test]
    async fn mock_payload_builder() -> eyre::Result<()> {
        let executor = TaskExecutor::current();
        let cfg_container =
            WithConfigs { config: NodeConfig::test(), toml_config: reth_config::Config::default() };

        let factory = OpNode::provider_factory_builder()
            .open_read_only(OP_MAINNET.clone(), "datadir")
            .unwrap();

        let provider = factory.provider().unwrap();

        let cb = ComponentsBuilder::default()
            .node_types::<RethFullAdapter<_, OpNode>>()
            .noop_pool::<OpPooledTransaction>()
            .executor(OpExecutorBuilder::default())
            .noop_consensus()
            .noop_network::<OpNetworkPrimitives>()
            // .payload::<OpPayloadBuilder>(op_payload_builder),
            .noop_payload();

        // cb.build_components(ctx);

        let da_config = OpDAConfig::new(42, 42);
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
        let sepolia = NodeConfig::new(OP_SEPOLIA.clone());
        // let db = create_test_rw_db_with_path(sepolia.datadir());
        // let dp = pb;
        let db = reth_revm::State::builder().build();

        let mut info = ExecutionInfo::new();
        //define mockPayloadTransactions
        let mut mock_builder = pb.block_builder(&mut db).unwrap();

        let aa = generate_many_transactions(10, 0, true);

        // let best_txs = OpPayloadTransactions::best_transactions(&self, pool, attr);

        let _ = pb.execute_best_transactions(&mut info, &mut mock_builder, aa).unwrap();

        // let ctx: BuilderContext =
        //     BuilderContext::new(Head::default(), provider, executor, cfg_container);

        // let pool = OpPoolBuilder::default().build_pool(&ctx, evm_config).await?;

        // let pb = OpPayloadBuilder::new(false)
        //     .with_transactions(best_transactions)
        //     .with_da_config(da_cfg)
        //     .build_payload_builder(&ctx, pool, evm_config)
        //     .await?;

        Ok(())

        // now we can write tests
    }

    #[tokio::test]
    async fn dd() -> eyre::Result<()> {
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
                                    Arc<
                                        Arc<
                                            reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>,
                                        >,
                                    >,
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
}
