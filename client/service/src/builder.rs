// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
	Service, NetworkStatus, NetworkState, error::Error, DEFAULT_PROTOCOL_ID, MallocSizeOfWasm,
	start_rpc_servers, build_network_future, TransactionPoolAdapter, TaskManager, SpawnTaskHandle,
	status_sinks, metrics::MetricsService, client::{Client, ClientConfig},
	config::{Configuration, KeystoreConfig, PrometheusConfig, OffchainWorkerConfig},
};
use sc_client_api::{
	BlockchainEvents, backend::RemoteBackend, light::RemoteBlockchain,
	execution_extensions::ExtensionsFactory, ExecutorProvider, CallExecutor, ForkBlocks, BadBlocks,
	CloneableSpawn, UsageProvider,
};
use sp_utils::mpsc::{tracing_unbounded, TracingUnboundedSender};
use sc_chain_spec::get_extension;
use sp_consensus::{
	block_validation::{BlockAnnounceValidator, DefaultBlockAnnounceValidator},
	import_queue::ImportQueue,
};
use futures::{
	Future, FutureExt, StreamExt,
	future::ready,
};
use jsonrpc_pubsub::manager::SubscriptionManager;
use sc_keystore::Store as Keystore;
use log::{info, warn, error};
use sc_network::config::{Role, FinalityProofProvider, OnDemand, BoxFinalityProofRequestBuilder};
use sc_network::{NetworkService, NetworkStateInfo};
use parking_lot::{Mutex, RwLock};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{
	Block as BlockT, NumberFor, SaturatedConversion, HashFor,
};
use sp_api::ProvideRuntimeApi;
use sc_executor::{NativeExecutor, NativeExecutionDispatch, RuntimeInfo};
use std::{
	collections::HashMap,
	io::{Read, Write, Seek},
	marker::PhantomData, sync::Arc, pin::Pin
};
use wasm_timer::SystemTime;
use sc_telemetry::{telemetry, SUBSTRATE_INFO};
use sp_transaction_pool::MaintainedTransactionPool;
use prometheus_endpoint::Registry;
use sc_client_db::{Backend, DatabaseSettings};
use sp_core::traits::CodeExecutor;
use sp_runtime::BuildStorage;
use sc_client_api::execution_extensions::ExecutionExtensions;
use sp_core::storage::Storage;

pub type BackgroundTask = Pin<Box<dyn Future<Output=()> + Send>>;

/// A utility trait for building an RPC extension given a `DenyUnsafe` instance.
/// This is useful since at service definition time we don't know whether the
/// specific interface where the RPC extension will be exposed is safe or not.
/// This trait allows us to lazily build the RPC extension whenever we bind the
/// service to an interface.
pub trait RpcExtensionBuilder {
	/// The type of the RPC extension that will be built.
	type Output: sc_rpc::RpcExtension<sc_rpc::Metadata>;

	/// Returns an instance of the RPC extension for a particular `DenyUnsafe`
	/// value, e.g. the RPC extension might not expose some unsafe methods.
	fn build(&self, deny: sc_rpc::DenyUnsafe) -> Self::Output;
}

impl<F, R> RpcExtensionBuilder for F where
	F: Fn(sc_rpc::DenyUnsafe) -> R,
	R: sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	type Output = R;

	fn build(&self, deny: sc_rpc::DenyUnsafe) -> Self::Output {
		(*self)(deny)
	}
}

/// A utility struct for implementing an `RpcExtensionBuilder` given a cloneable
/// `RpcExtension`, the resulting builder will simply ignore the provided
/// `DenyUnsafe` instance and return a static `RpcExtension` instance.
struct NoopRpcExtensionBuilder<R>(R);

impl<R> RpcExtensionBuilder for NoopRpcExtensionBuilder<R> where
	R: Clone + sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	type Output = R;

	fn build(&self, _deny: sc_rpc::DenyUnsafe) -> Self::Output {
		self.0.clone()
	}
}

impl<R> From<R> for NoopRpcExtensionBuilder<R> where
	R: sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	fn from(e: R) -> NoopRpcExtensionBuilder<R> {
		NoopRpcExtensionBuilder(e)
	}
}


/// Full client type.
pub type TFullClient<TBl, TRtApi, TExecDisp> = Client<
	TFullBackend<TBl>,
	TFullCallExecutor<TBl, TExecDisp>,
	TBl,
	TRtApi,
>;

/// Full client backend type.
pub type TFullBackend<TBl> = sc_client_db::Backend<TBl>;

/// Full client call executor type.
pub type TFullCallExecutor<TBl, TExecDisp> = crate::client::LocalCallExecutor<
	sc_client_db::Backend<TBl>,
	NativeExecutor<TExecDisp>,
>;

type TFullParts<TBl, TRtApi, TExecDisp> = (
	TFullClient<TBl, TRtApi, TExecDisp>,
	Arc<TFullBackend<TBl>>,
	Arc<RwLock<sc_keystore::Store>>,
	TaskManager,
);

/// Light client type.
pub type TLightClient<TBl, TRtApi, TExecDisp> = Arc<Client<
	TLightBackend<TBl>,
	TLightCallExecutor<TBl, TExecDisp>,
	TBl,
	TRtApi,
>>;

/// Light client backend type.
pub type TLightBackend<TBl> = crate::client::light::backend::Backend<
	sc_client_db::light::LightStorage<TBl>,
	HashFor<TBl>,
>;

/// Light call executor type.
pub type TLightCallExecutor<TBl, TExecDisp> = crate::client::light::call_executor::GenesisCallExecutor<
	crate::client::light::backend::Backend<
		sc_client_db::light::LightStorage<TBl>,
		HashFor<TBl>
	>,
	crate::client::LocalCallExecutor<
		crate::client::light::backend::Backend<
			sc_client_db::light::LightStorage<TBl>,
			HashFor<TBl>
		>,
		NativeExecutor<TExecDisp>
	>,
>;

type TLightParts<TBl, TRtApi, TExecDisp> = (
	TLightClient<TBl, TRtApi, TExecDisp>,
	Arc<TLightBackend<TBl>>,
	Arc<RwLock<sc_keystore::Store>>,
	TaskManager,
);

/// Creates a new full client for the given config.
pub fn new_full_client<TBl, TRtApi, TExecDisp>(
	config: &Configuration,
) -> Result<TFullClient<TBl, TRtApi, TExecDisp>, Error> where
	TBl: BlockT,
	TExecDisp: NativeExecutionDispatch + 'static,
{
	new_full_parts(config).map(|parts| parts.0)
}

pub fn new_full_parts<TBl, TRtApi, TExecDisp>(
	config: &Configuration,
) -> Result<TFullParts<TBl, TRtApi, TExecDisp>,	Error> where
	TBl: BlockT,
	TExecDisp: NativeExecutionDispatch + 'static,
{
	let keystore = match &config.keystore {
		KeystoreConfig::Path { path, password } => Keystore::open(
			path.clone(),
			password.clone()
		)?,
		KeystoreConfig::InMemory => Keystore::new_in_memory(),
	};

	let task_manager = {
		let registry = config.prometheus_config.as_ref().map(|cfg| &cfg.registry);
		TaskManager::new(config.task_executor.clone(), registry)?
	};

	let executor = NativeExecutor::<TExecDisp>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
	);

	let chain_spec = &config.chain_spec;
	let fork_blocks = get_extension::<ForkBlocks<TBl>>(chain_spec.extensions())
		.cloned()
		.unwrap_or_default();

	let bad_blocks = get_extension::<BadBlocks<TBl>>(chain_spec.extensions())
		.cloned()
		.unwrap_or_default();

	let (client, backend) = {
		let db_config = sc_client_db::DatabaseSettings {
			state_cache_size: config.state_cache_size,
			state_cache_child_ratio:
			config.state_cache_child_ratio.map(|v| (v, 100)),
			pruning: config.pruning.clone(),
			source: config.database.clone(),
		};

		let extensions = sc_client_api::execution_extensions::ExecutionExtensions::new(
			config.execution_strategies.clone(),
			Some(keystore.clone()),
		);

		new_client(
			db_config,
			executor,
			chain_spec.as_storage_builder(),
			fork_blocks,
			bad_blocks,
			extensions,
			Box::new(task_manager.spawn_handle()),
			config.prometheus_config.as_ref().map(|config| config.registry.clone()),
			ClientConfig {
				offchain_worker_enabled : config.offchain_worker.enabled ,
				offchain_indexing_api: config.offchain_worker.indexing_enabled,
			},
		)?
	};

	Ok((client, backend, keystore, task_manager))
}


/// Create an instance of db-backed client.
pub fn new_client<E, Block, RA>(
	settings: DatabaseSettings,
	executor: E,
	genesis_storage: &dyn BuildStorage,
	fork_blocks: ForkBlocks<Block>,
	bad_blocks: BadBlocks<Block>,
	execution_extensions: ExecutionExtensions<Block>,
	spawn_handle: Box<dyn CloneableSpawn>,
	prometheus_registry: Option<Registry>,
	config: ClientConfig,
) -> Result<(
	crate::client::Client<
		Backend<Block>,
		crate::client::LocalCallExecutor<Backend<Block>, E>,
		Block,
		RA,
	>,
	Arc<Backend<Block>>,
),
	sp_blockchain::Error,
>
	where
		Block: BlockT,
		E: CodeExecutor + RuntimeInfo,
{
	const CANONICALIZATION_DELAY: u64 = 4096;

	let backend = Arc::new(Backend::new(settings, CANONICALIZATION_DELAY)?);
	let executor = crate::client::LocalCallExecutor::new(backend.clone(), executor, spawn_handle, config.clone());
	Ok((
		crate::client::Client::new(
			backend.clone(),
			executor,
			genesis_storage,
			fork_blocks,
			bad_blocks,
			execution_extensions,
			prometheus_registry,
			config,
		)?,
		backend,
	))
}

/// Start the service builder with a configuration.
pub fn new_light_parts<TBl: BlockT, TRtApi, TExecDisp: NativeExecutionDispatch + 'static>(
	config: &Configuration,
) -> Result<(TLightParts<TBl, TRtApi, TExecDisp>, Arc<OnDemand<TBl>>, Arc<dyn RemoteBlockchain<TBl>>), Error> {
	let task_manager = {
		let registry = config.prometheus_config.as_ref().map(|cfg| &cfg.registry);
		TaskManager::new(config.task_executor.clone(), registry)?
	};

	let keystore = match &config.keystore {
		KeystoreConfig::Path { path, password } => Keystore::open(
			path.clone(),
			password.clone()
		)?,
		KeystoreConfig::InMemory => Keystore::new_in_memory(),
	};

	let executor = NativeExecutor::<TExecDisp>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
	);

	let db_storage = {
		let db_settings = sc_client_db::DatabaseSettings {
			state_cache_size: config.state_cache_size,
			state_cache_child_ratio:
				config.state_cache_child_ratio.map(|v| (v, 100)),
			pruning: config.pruning.clone(),
			source: config.database.clone(),
		};
		sc_client_db::light::LightStorage::new(db_settings)?
	};
	let light_blockchain = crate::client::light::new_light_blockchain(db_storage);
	let fetch_checker = Arc::new(
		crate::client::light::new_fetch_checker::<_, TBl, _>(
			light_blockchain.clone(),
			executor.clone(),
			Box::new(task_manager.spawn_handle()),
		),
	);
	let fetcher = Arc::new(sc_network::config::OnDemand::new(fetch_checker));
	let backend = crate::client::light::new_light_backend(light_blockchain);
	let remote_blockchain = backend.remote_blockchain();
	let client = Arc::new(crate::client::light::new_light(
		backend.clone(),
		config.chain_spec.as_storage_builder(),
		executor,
		Box::new(task_manager.spawn_handle()),
		config.prometheus_config.as_ref().map(|config| config.registry.clone()),
	)?);

	Ok(((client, backend, keystore, task_manager), fetcher, remote_blockchain))
}

pub struct ServiceDescriptor<TBl: BlockT, TRtApi, TSc, TImpQu, TExPool, TRpc, TBackend, TExec, URpcBuilder> {
	pub config: Configuration,
	pub client: Arc<Client<TBackend, TExec, TBl, TRtApi>,>,
	pub backend: Arc<TBackend>,
	pub task_manager: TaskManager,
	pub keystore: Arc<RwLock<Keystore>>,
	pub on_demand: Option<Arc<OnDemand<TBl>>>,
	pub select_chain: Option<TSc>,
	pub import_queue: TImpQu,
	pub finality_proof_request_builder: Option<BoxFinalityProofRequestBuilder<TBl>>,
	pub finality_proof_provider: Option<Arc<dyn FinalityProofProvider<TBl>>>,
	pub transaction_pool: Arc<TExPool>,
	pub rpc_extensions: TRpc,
	pub remote_blockchain: Option<Arc<dyn RemoteBlockchain<TBl>>>,
	pub background_tasks: Vec<(&'static str, BackgroundTask)>,
	pub block_announce_validator_builder: Option<Box<dyn FnOnce(Arc<Client<TBackend, TExec, TBl, TRtApi>,>) -> Box<dyn BlockAnnounceValidator<TBl> + Send> + Send + 'static>>,
	pub rpc_extensions_builder: URpcBuilder,
	pub informant_prefix: String,
}

/// Builds the service.
pub fn build<
	TBl,
	TRtApi,
	TSc,
	TImpQu,
	TExPool,
	TRpc,
	TBackend,
	TExec,
	URpcBuilder,
	URpc,
>(
	service_descriptor: ServiceDescriptor<TBl, TRtApi, TSc, TImpQu, TExPool, TRpc, TBackend, TExec, URpcBuilder>,
) -> Result<Service<
	TBl,
	Client<TBackend, TExec, TBl, TRtApi>,
	TSc,
	NetworkStatus<TBl>,
	NetworkService<TBl, <TBl as BlockT>::Hash>,
	TExPool,
	sc_offchain::OffchainWorkers<
		Client<TBackend, TExec, TBl, TRtApi>,
		TBackend::OffchainStorage,
		TBl
	>,
>, Error>
where
	Client<TBackend, TExec, TBl, TRtApi>: ProvideRuntimeApi<TBl>,
	<Client<TBackend, TExec, TBl, TRtApi> as ProvideRuntimeApi<TBl>>::Api:
		sp_api::Metadata<TBl> +
		sc_offchain::OffchainWorkerApi<TBl> +
		sp_transaction_pool::runtime_api::TaggedTransactionQueue<TBl> +
		sp_session::SessionKeys<TBl> +
		sp_api::ApiErrorExt<Error = sp_blockchain::Error> +
		sp_api::ApiExt<TBl, StateBackend = TBackend::State>,
	TBl: BlockT,
	TRtApi: 'static + Send + Sync,
	TBackend: 'static + sc_client_api::backend::Backend<TBl> + Send,
	TExec: 'static + CallExecutor<TBl> + Send + Sync + Clone,
	TSc: Clone,
	TImpQu: 'static + ImportQueue<TBl>,
	TExPool: MaintainedTransactionPool<Block=TBl, Hash = <TBl as BlockT>::Hash> + MallocSizeOfWasm + 'static,
	TRpc: sc_rpc::RpcExtension<sc_rpc::Metadata>,
	TExec: CallExecutor<TBl, Backend = TBackend>,
	URpcBuilder: RpcExtensionBuilder<Output = URpc> + Send + 'static,
	URpc: sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	let ServiceDescriptor {
		mut config,
		client, backend, task_manager, keystore, on_demand, select_chain, import_queue, finality_proof_request_builder, finality_proof_provider, transaction_pool, rpc_extensions, 
		remote_blockchain, background_tasks, block_announce_validator_builder, rpc_extensions_builder, informant_prefix
	} = service_descriptor;

	sp_session::generate_initial_session_keys(
		client.clone(),
		&BlockId::Hash(client.chain_info().best_hash),
		config.dev_key_seed.clone().map(|s| vec![s]).unwrap_or_default(),
	)?;

	// A side-channel for essential tasks to communicate shutdown.
	let (essential_failed_tx, essential_failed_rx) = tracing_unbounded("mpsc_essential_tasks");

	sp_session::generate_initial_session_keys(
		client.clone(),
		&BlockId::Hash(client.chain_info().best_hash),
		config.dev_key_seed.clone().map(|s| vec![s]).unwrap_or_default(),
	)?;

	let import_queue = Box::new(import_queue);
	let chain_info = client.chain_info();
	let chain_spec = &config.chain_spec;

	let version = config.impl_version;
	info!("📦 Highest known block at #{}", chain_info.best_number);
	telemetry!(
		SUBSTRATE_INFO;
		"node.start";
		"height" => chain_info.best_number.saturated_into::<u64>(),
		"best" => ?chain_info.best_hash
	);

	// make transaction pool available for off-chain runtime calls.
	client.execution_extensions()
		.register_transaction_pool(Arc::downgrade(&transaction_pool) as _);

	let transaction_pool_adapter = Arc::new(TransactionPoolAdapter {
		imports_external_transactions: !matches!(config.role, Role::Light),
		pool: transaction_pool.clone(),
		client: client.clone(),
	});

	let protocol_id = {
		let protocol_id_full = match chain_spec.protocol_id() {
			Some(pid) => pid,
			None => {
				warn!("Using default protocol ID {:?} because none is configured in the \
					chain specs", DEFAULT_PROTOCOL_ID
				);
				DEFAULT_PROTOCOL_ID
			}
		}.as_bytes();
		sc_network::config::ProtocolId::from(protocol_id_full)
	};

	let block_announce_validator = if let Some(f) = block_announce_validator_builder {
		f(client.clone())
	} else {
		Box::new(DefaultBlockAnnounceValidator::new(client.clone()))
	};

	let network_params = sc_network::config::Params {
		role: config.role.clone(),
		executor: {
			let spawn_handle = task_manager.spawn_handle();
			Some(Box::new(move |fut| {
				spawn_handle.spawn("libp2p-node", fut);
			}))
		},
		network_config: config.network.clone(),
		chain: client.clone(),
		finality_proof_provider,
		finality_proof_request_builder,
		on_demand: on_demand.clone(),
		transaction_pool: transaction_pool_adapter.clone() as _,
		import_queue,
		protocol_id,
		block_announce_validator,
		metrics_registry: config.prometheus_config.as_ref().map(|config| config.registry.clone())
	};

	let has_bootnodes = !network_params.network_config.boot_nodes.is_empty();
	let network_mut = sc_network::NetworkWorker::new(network_params)?;
	let network = network_mut.service().clone();
	let network_status_sinks = Arc::new(Mutex::new(status_sinks::StatusSinks::new()));

	let offchain_storage = backend.offchain_storage();
	let offchain_workers = match (config.offchain_worker.clone(), offchain_storage.clone()) {
		(OffchainWorkerConfig {enabled: true, .. }, Some(db)) => {
			Some(Arc::new(sc_offchain::OffchainWorkers::new(client.clone(), db)))
		},
		(OffchainWorkerConfig {enabled: true, .. }, None) => {
			warn!("Offchain workers disabled, due to lack of offchain storage support in backend.");
			None
		},
		_ => None,
	};

	let spawn_handle = task_manager.spawn_handle();

	// Spawn background tasks which were stacked during the
	// service building.
	for (title, background_task) in background_tasks {
		spawn_handle.spawn(title, background_task);
	}

	// Inform the tx pool about imported and finalized blocks.
	{
		let txpool = Arc::downgrade(&transaction_pool);

		let mut import_stream = client.import_notification_stream().map(Into::into).fuse();
		let mut finality_stream = client.finality_notification_stream()
			.map(Into::into)
			.fuse();

		let events = async move {
			loop {
				let evt = futures::select! {
					evt = import_stream.next() => evt,
					evt = finality_stream.next() => evt,
					complete => return,
				};

				let txpool = txpool.upgrade();
				if let Some((txpool, evt)) = txpool.and_then(|tp| evt.map(|evt| (tp, evt))) {
					txpool.maintain(evt).await;
				}
			}
		};

		spawn_handle.spawn(
			"txpool-notifications",
			events,
		);
	}

	{
		// extrinsic notifications
		let network = Arc::downgrade(&network);
		let transaction_pool_ = transaction_pool.clone();
		let events = transaction_pool.import_notification_stream()
			.for_each(move |hash| {
				if let Some(network) = network.upgrade() {
					network.propagate_extrinsic(hash);
				}
				let status = transaction_pool_.status();
				telemetry!(SUBSTRATE_INFO; "txpool.import";
					"ready" => status.ready,
					"future" => status.future
				);
				ready(())
			});

		spawn_handle.spawn(
			"telemetry-on-block",
			events,
		);
	}

	// Prometheus metrics.
	let mut metrics_service = if let Some(PrometheusConfig { port, registry }) = config.prometheus_config.clone() {
		// Set static metrics.


		let role_bits = match config.role {
			Role::Full => 1u64,
			Role::Light => 2u64,
			Role::Sentry { .. } => 3u64,
			Role::Authority { .. } => 4u64,
		};
		let metrics = MetricsService::with_prometheus(
			&registry,
			&config.network.node_name,
			&config.impl_version,
			role_bits,
		)?;
		spawn_handle.spawn(
			"prometheus-endpoint",
			prometheus_endpoint::init_prometheus(port, registry).map(drop)
		);

		metrics
	} else {
		MetricsService::new()
	};

	// Periodically notify the telemetry.
	let transaction_pool_ = transaction_pool.clone();
	let client_ = client.clone();
	let (state_tx, state_rx) = tracing_unbounded::<(NetworkStatus<_>, NetworkState)>("mpsc_netstat1");
	network_status_sinks.lock().push(std::time::Duration::from_millis(5000), state_tx);
	let tel_task = state_rx.for_each(move |(net_status, _)| {
		let info = client_.usage_info();
		metrics_service.tick(
			&info,
			&transaction_pool_.status(),
			&net_status,
		);
		ready(())
	});

	spawn_handle.spawn(
		"telemetry-periodic-send",
		tel_task,
	);

	// Periodically send the network state to the telemetry.
	let (netstat_tx, netstat_rx) = tracing_unbounded::<(NetworkStatus<_>, NetworkState)>("mpsc_netstat2");
	network_status_sinks.lock().push(std::time::Duration::from_secs(30), netstat_tx);
	let tel_task_2 = netstat_rx.for_each(move |(_, network_state)| {
		telemetry!(
			SUBSTRATE_INFO;
			"system.network_state";
			"state" => network_state,
		);
		ready(())
	});
	spawn_handle.spawn(
		"telemetry-periodic-network-state",
		tel_task_2,
	);

	// RPC
	let (system_rpc_tx, system_rpc_rx) = tracing_unbounded("mpsc_system_rpc");
	let gen_handler = |deny_unsafe: sc_rpc::DenyUnsafe| {
	use sc_rpc::{chain, state, author, system, offchain};

	let system_info = sc_rpc::system::SystemInfo {
		chain_name: chain_spec.name().into(),
		impl_name: config.impl_name.into(),
		impl_version: config.impl_version.into(),
		properties: chain_spec.properties().clone(),
		chain_type: chain_spec.chain_type().clone(),
	};

	let subscriptions = SubscriptionManager::new(Arc::new(task_manager.spawn_handle()));

	let (chain, state, child_state) = if let (Some(remote_blockchain), Some(on_demand)) =
		(remote_blockchain.as_ref(), on_demand.as_ref()) {
		// Light clients
		let chain = sc_rpc::chain::new_light(
			client.clone(),
			subscriptions.clone(),
			remote_blockchain.clone(),
			on_demand.clone()
		);
		let (state, child_state) = sc_rpc::state::new_light(
			client.clone(),
			subscriptions.clone(),
			remote_blockchain.clone(),
			on_demand.clone()
		);
		(chain, state, child_state)

	} else {
		// Full nodes
		let chain = sc_rpc::chain::new_full(client.clone(), subscriptions.clone());
		let (state, child_state) = sc_rpc::state::new_full(client.clone(), subscriptions.clone());
		(chain, state, child_state)
	};

	let author = sc_rpc::author::Author::new(
		client.clone(),
		transaction_pool.clone(),
		subscriptions,
		keystore.clone(),
		deny_unsafe,
	);
	let system = system::System::new(system_info, system_rpc_tx.clone(), deny_unsafe);

	let maybe_offchain_rpc = offchain_storage.clone()
	.map(|storage| {
		let offchain = sc_rpc::offchain::Offchain::new(storage, deny_unsafe);
		// FIXME: Use plain Option (don't collect into HashMap) when we upgrade to jsonrpc 14.1
		// https://github.com/paritytech/jsonrpc/commit/20485387ed06a48f1a70bf4d609a7cde6cf0accf
		let delegate = offchain::OffchainApi::to_delegate(offchain);
			delegate.into_iter().collect::<HashMap<_, _>>()
	}).unwrap_or_default();

	sc_rpc_server::rpc_handler((
		state::StateApi::to_delegate(state),
		state::ChildStateApi::to_delegate(child_state),
		chain::ChainApi::to_delegate(chain),
		maybe_offchain_rpc,
		author::AuthorApi::to_delegate(author),
		system::SystemApi::to_delegate(system),
		rpc_extensions_builder.build(deny_unsafe),
	))
};
	let rpc = start_rpc_servers(&config, gen_handler)?;
	// This is used internally, so don't restrict access to unsafe RPC
	let rpc_handlers = gen_handler(sc_rpc::DenyUnsafe::No);

	// The network worker is responsible for gathering all network messages and processing
	// them. This is quite a heavy task, and at the time of the writing of this comment it
	// frequently happens that this future takes several seconds or in some situations
	// even more than a minute until it has processed its entire queue. This is clearly an
	// issue, and ideally we would like to fix the network future to take as little time as
	// possible, but we also take the extra harm-prevention measure to execute the networking
	// future using `spawn_blocking`.
	spawn_handle.spawn_blocking(
		"network-worker",
		build_network_future(
			config.role.clone(),
			network_mut,
			client.clone(),
			network_status_sinks.clone(),
			system_rpc_rx,
			has_bootnodes,
			config.announce_block,
		),
	);

	let telemetry_connection_sinks: Arc<Mutex<Vec<TracingUnboundedSender<()>>>> = Default::default();

	// Telemetry
	let telemetry = config.telemetry_endpoints.clone().map(|endpoints| {
		let is_authority = config.role.is_authority();
		let network_id = network.local_peer_id().to_base58();
		let name = config.network.node_name.clone();
		let impl_name = config.impl_name.to_owned();
		let version = version.clone();
		let chain_name = config.chain_spec.name().to_owned();
		let telemetry_connection_sinks_ = telemetry_connection_sinks.clone();
		let telemetry = sc_telemetry::init_telemetry(sc_telemetry::TelemetryConfig {
			endpoints,
			wasm_external_transport: config.telemetry_external_transport.take(),
		});
		let startup_time = SystemTime::UNIX_EPOCH.elapsed()
			.map(|dur| dur.as_millis())
			.unwrap_or(0);
		let future = telemetry.clone()
			.for_each(move |event| {
				// Safe-guard in case we add more events in the future.
				let sc_telemetry::TelemetryEvent::Connected = event;

				telemetry!(SUBSTRATE_INFO; "system.connected";
					"name" => name.clone(),
					"implementation" => impl_name.clone(),
					"version" => version.clone(),
					"config" => "",
					"chain" => chain_name.clone(),
					"authority" => is_authority,
					"startup_time" => startup_time,
					"network_id" => network_id.clone()
				);

				telemetry_connection_sinks_.lock().retain(|sink| {
					sink.unbounded_send(()).is_ok()
				});
				ready(())
			});

		spawn_handle.spawn(
			"telemetry-worker",
			future,
		);

		telemetry
	});

	// Instrumentation
	if let Some(tracing_targets) = config.tracing_targets.as_ref() {
		let subscriber = sc_tracing::ProfilingSubscriber::new(
			config.tracing_receiver, tracing_targets
		);
		match tracing::subscriber::set_global_default(subscriber) {
			Ok(_) => (),
			Err(e) => error!(target: "tracing", "Unable to set global default subscriber {}", e),
		}
	}

	// Spawn informant task
	let network_status_sinks_1 = network_status_sinks.clone();
	let informant_future = sc_informant::build(
		client.clone(),
		move |interval| {
			let (sink, stream) = tracing_unbounded("mpsc_network_status");
			network_status_sinks_1.lock().push(interval, sink);
			stream
		},
		transaction_pool.clone(),
		sc_informant::OutputFormat { enable_color: true, prefix: informant_prefix },
	);
	spawn_handle.spawn("informant", informant_future);

	Ok(Service {
		client,
		task_manager,
		network,
		network_status_sinks,
		select_chain,
		transaction_pool,
		essential_failed_tx,
		essential_failed_rx,
		rpc_handlers,
		_rpc: rpc,
		_telemetry: telemetry,
		_offchain_workers: offchain_workers,
		_telemetry_on_connect_sinks: telemetry_connection_sinks.clone(),
		keystore,
		marker: PhantomData::<TBl>,
		prometheus_registry: config.prometheus_config.map(|config| config.registry),
	})
}
