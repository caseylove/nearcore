use std::sync::{Arc, Mutex, RwLock};

use near_async::futures::FutureSpawner;
use near_async::messaging::{noop, IntoMultiSender, IntoSender, LateBoundSender};
use near_async::test_loop::sender::TestLoopSender;
use near_async::test_loop::TestLoopV2;
use near_async::time::{Clock, Duration};
use near_chain::chunks_store::ReadOnlyChunksStore;
use near_chain::runtime::NightshadeRuntime;
use near_chain::state_snapshot_actor::{
    get_delete_snapshot_callback, get_make_snapshot_callback, SnapshotCallbacks, StateSnapshotActor,
};
use near_chain::types::RuntimeAdapter;
use near_chain::ChainGenesis;
use near_chain_configs::{
    ClientConfig, DumpConfig, ExternalStorageConfig, ExternalStorageLocation, Genesis,
    MutableConfigValue, StateSyncConfig, SyncConfig,
};
use near_chunks::shards_manager_actor::ShardsManagerActor;
use near_client::client_actor::ClientActorInner;
use near_client::gc_actor::GCActor;
use near_client::sync_jobs_actor::SyncJobsActor;
use near_client::test_utils::test_loop::test_loop_sync_actor_maker;
use near_client::{Client, PartialWitnessActor, SyncAdapter};
use near_epoch_manager::shard_tracker::{ShardTracker, TrackedConfig};
use near_epoch_manager::{EpochManager, EpochManagerAdapter};
use near_network::test_loop::TestLoopPeerManagerActor;
use near_primitives::network::PeerId;
use near_primitives::test_utils::create_test_signer;
use near_primitives::types::AccountId;
use near_store::config::StateSnapshotType;
use near_store::genesis::initialize_genesis_state;
use near_store::test_utils::create_test_store;
use near_store::{StoreConfig, TrieConfig};
use near_vm_runner::{ContractRuntimeCache, FilesystemContractRuntimeCache};
use nearcore::state_sync::StateSyncDumper;
use tempfile::TempDir;

use super::env::{ClientToShardsManagerSender, TestData, TestLoopChunksStorage, TestLoopEnv};
use super::utils::network::partial_encoded_chunks_dropper;

pub struct TestLoopBuilder {
    test_loop: TestLoopV2,
    genesis: Option<Genesis>,
    clients: Vec<AccountId>,
    /// Will store all chunks produced within the test loop.
    chunks_storage: Arc<Mutex<TestLoopChunksStorage>>,
    /// Whether test loop should drop all chunks validated by the given account.
    drop_chunks_validated_by: Option<AccountId>,
    gc: bool,
}

impl TestLoopBuilder {
    pub fn new() -> Self {
        Self {
            test_loop: TestLoopV2::new(),
            genesis: None,
            clients: vec![],
            chunks_storage: Default::default(),
            drop_chunks_validated_by: None,
            gc: true,
        }
    }

    /// Get the clock for the test loop.
    pub fn clock(&self) -> Clock {
        self.test_loop.clock()
    }

    /// Set the genesis configuration for the test loop.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.genesis = Some(genesis);
        self
    }

    /// Set the clients for the test loop.
    pub fn clients(mut self, clients: Vec<AccountId>) -> Self {
        self.clients = clients;
        self
    }

    pub fn drop_chunks_validated_by(mut self, account_id: &str) -> Self {
        self.drop_chunks_validated_by = Some(account_id.parse().unwrap());
        self
    }

    /// Build the test loop environment.
    pub fn build(self) -> TestLoopEnv {
        self.ensure_genesis().ensure_clients().build_impl()
    }

    fn ensure_genesis(self) -> Self {
        assert!(self.genesis.is_some(), "Genesis must be provided to the test loop");
        self
    }

    fn ensure_clients(self) -> Self {
        assert!(!self.clients.is_empty(), "Clients must be provided to the test loop");
        self
    }

    fn build_impl(mut self) -> TestLoopEnv {
        let mut datas = Vec::new();
        let mut network_adapters = Vec::new();
        let mut epoch_manager_adapters = Vec::new();
        let tempdir = tempfile::tempdir().unwrap();
        for idx in 0..self.clients.len() {
            let (data, network_adapter, epoch_manager_adapter) = self.setup_client(idx, &tempdir);
            datas.push(data);
            network_adapters.push(network_adapter);
            epoch_manager_adapters.push(epoch_manager_adapter);
        }
        self.setup_network(&datas, &network_adapters, &epoch_manager_adapters);

        let env = TestLoopEnv { test_loop: self.test_loop, datas, tempdir };
        env.warmup()
    }

    fn setup_client(
        &mut self,
        idx: usize,
        tempdir: &TempDir,
    ) -> (
        TestData,
        Arc<LateBoundSender<TestLoopSender<TestLoopPeerManagerActor>>>,
        Arc<dyn EpochManagerAdapter>,
    ) {
        let client_adapter = LateBoundSender::new();
        let network_adapter = LateBoundSender::new();
        let state_snapshot_adapter = LateBoundSender::new();
        let partial_witness_adapter = LateBoundSender::new();
        let sync_jobs_adapter = LateBoundSender::new();

        let genesis = self.genesis.clone().unwrap();
        let mut client_config = ClientConfig::test(true, 600, 2000, 4, false, true, false, false);
        client_config.max_block_wait_delay = Duration::seconds(6);
        client_config.state_sync_enabled = true;
        client_config.state_sync_timeout = Duration::milliseconds(100);
        let external_storage_location =
            ExternalStorageLocation::Filesystem { root_dir: tempdir.path().join("state_sync") };
        client_config.state_sync = StateSyncConfig {
            dump: Some(DumpConfig {
                iteration_delay: Some(Duration::seconds(1)),
                location: external_storage_location.clone(),
                credentials_file: None,
                restart_dump_for_shards: None,
            }),
            sync: SyncConfig::ExternalStorage(ExternalStorageConfig {
                location: external_storage_location,
                num_concurrent_requests: 1,
                num_concurrent_requests_during_catchup: 1,
            }),
        };

        // Configure tracked shards.
        // * single shard tracking for validators
        // * all shard tracking for RPCs
        let num_block_producer = genesis.config.num_block_producer_seats;
        let num_chunk_producer = genesis.config.num_chunk_producer_seats;
        let num_chunk_validator = genesis.config.num_chunk_validator_seats;
        let validator_num =
            num_block_producer.max(num_chunk_producer).max(num_chunk_validator) as usize;
        if idx < validator_num {
            client_config.tracked_shards = Vec::new();
        } else {
            client_config.tracked_shards = vec![666];
        }

        let homedir = tempdir.path().join(format!("{}", idx));
        std::fs::create_dir_all(&homedir).expect("Unable to create homedir");

        let store_config = StoreConfig {
            path: Some(homedir.clone()),
            load_mem_tries_for_tracked_shards: true,
            ..Default::default()
        };
        let store = create_test_store();
        initialize_genesis_state(store.clone(), &genesis, None);

        let sync_jobs_actor = SyncJobsActor::new(client_adapter.as_multi_sender());
        let chain_genesis = ChainGenesis::new(&genesis.config);
        let epoch_manager = EpochManager::new_arc_handle(store.clone(), &genesis.config);
        let shard_tracker =
            ShardTracker::new(TrackedConfig::from_config(&client_config), epoch_manager.clone());

        let state_sync_adapter = Arc::new(RwLock::new(SyncAdapter::new(
            client_adapter.as_sender(),
            network_adapter.as_sender(),
            test_loop_sync_actor_maker(idx, self.test_loop.sender().for_index(idx)),
        )));
        let contract_cache = FilesystemContractRuntimeCache::new(&homedir, None::<&str>)
            .expect("filesystem contract cache")
            .handle();
        let runtime_adapter = NightshadeRuntime::test_with_trie_config(
            &homedir,
            store.clone(),
            contract_cache,
            &genesis.config,
            epoch_manager.clone(),
            None,
            TrieConfig::from_store_config(&store_config),
            StateSnapshotType::EveryEpoch,
        );

        let state_snapshot = StateSnapshotActor::new(
            runtime_adapter.get_flat_storage_manager(),
            network_adapter.as_multi_sender(),
            runtime_adapter.get_tries(),
            state_snapshot_adapter.as_multi_sender(),
        );

        let delete_snapshot_callback =
            get_delete_snapshot_callback(state_snapshot_adapter.as_multi_sender());
        let make_snapshot_callback = get_make_snapshot_callback(
            state_snapshot_adapter.as_multi_sender(),
            runtime_adapter.get_flat_storage_manager(),
        );
        let snapshot_callbacks =
            SnapshotCallbacks { make_snapshot_callback, delete_snapshot_callback };

        let validator_signer = MutableConfigValue::new(
            Some(Arc::new(create_test_signer(self.clients[idx].as_str()))),
            "validator_signer",
        );

        let shards_manager_adapter = LateBoundSender::new();
        let client_to_shards_manager_sender = Arc::new(ClientToShardsManagerSender {
            sender: shards_manager_adapter.clone(),
            chunks_storage: self.chunks_storage.clone(),
        });

        let client = Client::new(
            self.test_loop.clock(),
            client_config.clone(),
            chain_genesis.clone(),
            epoch_manager.clone(),
            shard_tracker.clone(),
            state_sync_adapter,
            runtime_adapter.clone(),
            network_adapter.as_multi_sender(),
            client_to_shards_manager_sender.as_sender(),
            validator_signer.clone(),
            true,
            [0; 32],
            Some(snapshot_callbacks),
            Arc::new(self.test_loop.async_computation_spawner(|_| Duration::milliseconds(80))),
            partial_witness_adapter.as_multi_sender(),
        )
        .unwrap();

        let shards_manager = ShardsManagerActor::new(
            self.test_loop.clock(),
            validator_signer.clone(),
            epoch_manager.clone(),
            shard_tracker.clone(),
            network_adapter.as_sender(),
            client_adapter.as_sender(),
            ReadOnlyChunksStore::new(store.clone()),
            client.chain.head().unwrap(),
            client.chain.header_head().unwrap(),
            Duration::milliseconds(100),
        );

        let client_actor = ClientActorInner::new(
            self.test_loop.clock(),
            client,
            client_adapter.as_multi_sender(),
            client_config.clone(),
            PeerId::random(),
            network_adapter.as_multi_sender(),
            noop().into_sender(),
            None,
            Default::default(),
            None,
            sync_jobs_adapter.as_multi_sender(),
            Box::new(self.test_loop.future_spawner()),
        )
        .unwrap();

        let partial_witness_actor = PartialWitnessActor::new(
            self.test_loop.clock(),
            network_adapter.as_multi_sender(),
            client_adapter.as_multi_sender(),
            validator_signer.clone(),
            epoch_manager.clone(),
            store,
        );

        if self.gc {
            let gc_actor = GCActor::new(
                runtime_adapter.store().clone(),
                chain_genesis.height,
                runtime_adapter.clone(),
                epoch_manager.clone(),
                client_config.gc.clone(),
                client_config.archive,
            );
            // We don't send messages to `GCActor` so adapter is not needed.
            self.test_loop.register_actor_for_index(idx, gc_actor, None);
        }

        let future_spawner = self.test_loop.future_spawner();
        let state_sync_dumper = StateSyncDumper {
            clock: self.test_loop.clock(),
            client_config,
            chain_genesis,
            epoch_manager: epoch_manager.clone(),
            shard_tracker,
            runtime: runtime_adapter,
            validator: validator_signer,
            dump_future_runner: Box::new(move |future| {
                future_spawner.spawn_boxed("state_sync_dumper", future);
                Box::new(|| {})
            }),
            handle: None,
        };
        let state_sync_dumper_handle = self.test_loop.data.register_data(state_sync_dumper);

        let client_sender =
            self.test_loop.register_actor_for_index(idx, client_actor, Some(client_adapter));
        let shards_manager_sender = self.test_loop.register_actor_for_index(
            idx,
            shards_manager,
            Some(shards_manager_adapter),
        );
        let partial_witness_sender = self.test_loop.register_actor_for_index(
            idx,
            partial_witness_actor,
            Some(partial_witness_adapter),
        );
        self.test_loop.register_actor_for_index(idx, sync_jobs_actor, Some(sync_jobs_adapter));
        self.test_loop.register_actor_for_index(idx, state_snapshot, Some(state_snapshot_adapter));

        // State sync dumper is not an Actor, handle starting separately.
        let state_sync_dumper_handle_clone = state_sync_dumper_handle.clone();
        self.test_loop.send_adhoc_event(
            "start_state_sync_dumper".to_owned(),
            move |test_loop_data| {
                test_loop_data.get_mut(&state_sync_dumper_handle_clone).start().unwrap();
            },
        );

        let data = TestData {
            account_id: self.clients[idx].clone(),
            client_sender,
            shards_manager_sender,
            partial_witness_sender,
            state_sync_dumper_handle,
        };
        (data, network_adapter, epoch_manager)
    }

    // TODO: we assume that all `Vec`s have the same length, consider
    // joining them into one structure.
    fn setup_network(
        &mut self,
        datas: &Vec<TestData>,
        network_adapters: &Vec<Arc<LateBoundSender<TestLoopSender<TestLoopPeerManagerActor>>>>,
        epoch_manager_adapters: &Vec<Arc<dyn EpochManagerAdapter>>,
    ) {
        for (idx, data) in datas.iter().enumerate() {
            let mut peer_manager_actor =
                TestLoopPeerManagerActor::new(self.test_loop.clock(), &data.account_id, datas);

            if let Some(account_id) = &self.drop_chunks_validated_by {
                peer_manager_actor.register_override_handler(partial_encoded_chunks_dropper(
                    self.chunks_storage.clone(),
                    epoch_manager_adapters[idx].clone(),
                    account_id.clone(),
                ));
            }

            self.test_loop.register_actor_for_index(
                idx,
                peer_manager_actor,
                Some(network_adapters[idx].clone()),
            );
        }
    }
}
