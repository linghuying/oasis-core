//! Runtime call dispatcher.
use std::{
    convert::TryInto,
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
};

use anyhow::{anyhow, Result as AnyResult};
use crossbeam::channel;
use io_context::Context;
use slog::{debug, error, info, warn, Logger};

use crate::{
    common::{
        crypto::{
            hash::Hash,
            signature::{Signature, Signer},
        },
        logger::get_logger,
    },
    consensus::{
        beacon::EpochTime,
        roothash::{self, ComputeResultsHeader, Header, COMPUTE_RESULTS_HEADER_CONTEXT},
        verifier::Verifier,
        LightBlock,
    },
    enclave_rpc::{
        demux::Demux as RpcDemux,
        dispatcher::Dispatcher as RpcDispatcher,
        types::{Message as RpcMessage, Request as RpcRequest},
        Context as RpcContext,
    },
    protocol::{Protocol, ProtocolUntrustedLocalStorage},
    rak::RAK,
    storage::mkvs::{
        sync::{HostReadSyncer, NoopReadSyncer},
        OverlayTree, Root, RootType, Tree,
    },
    transaction::{
        dispatcher::{Dispatcher as TxnDispatcher, NoopDispatcher as TxnNoopDispatcher},
        tree::Tree as TxnTree,
        types::TxnBatch,
        Context as TxnContext,
    },
    types::{Body, ComputedBatch, Error, HostStorageEndpoint},
};

/// Maximum amount of requests that can be in the dispatcher queue.
const BACKLOG_SIZE: usize = 1000;

/// Interface for dispatcher initializers.
pub trait Initializer: Send + Sync {
    /// Initializes the dispatcher(s).
    fn init(
        &self,
        protocol: &Arc<Protocol>,
        rak: &Arc<RAK>,
        rpc_demux: &mut RpcDemux,
        rpc_dispatcher: &mut RpcDispatcher,
    ) -> Option<Box<dyn TxnDispatcher>>;
}

impl<F> Initializer for F
where
    F: Fn(
            &Arc<Protocol>,
            &Arc<RAK>,
            &mut RpcDemux,
            &mut RpcDispatcher,
        ) -> Option<Box<dyn TxnDispatcher>>
        + Send
        + Sync,
{
    fn init(
        &self,
        protocol: &Arc<Protocol>,
        rak: &Arc<RAK>,
        rpc_demux: &mut RpcDemux,
        rpc_dispatcher: &mut RpcDispatcher,
    ) -> Option<Box<dyn TxnDispatcher>> {
        (*self)(protocol, rak, rpc_demux, rpc_dispatcher)
    }
}

type QueueItem = (Context, u64, Body);

/// A guard that will abort the process if dropped while panicking.
///
/// This is to ensure that the runtime will terminate in case there is
/// a panic encountered during dispatch and the runtime is built with
/// a non-abort panic handler.
struct AbortOnPanic;

impl Drop for AbortOnPanic {
    fn drop(&mut self) {
        if thread::panicking() {
            process::abort();
        }
    }
}

/// State related to dispatching a runtime transaction.
struct TxDispatchState<'a> {
    tokio_rt: &'a tokio::runtime::Runtime,
    consensus_block: LightBlock,
    consensus_verifier: &'a Box<dyn Verifier>,
    header: Header,
    epoch: EpochTime,
    round_results: roothash::RoundResults,
    max_messages: u32,
    check_only: bool,
}

/// State provided by the protocol upon successful initialization.
struct State {
    protocol: Arc<Protocol>,
    consensus_verifier: Box<dyn Verifier>,
}

/// Runtime call dispatcher.
pub struct Dispatcher {
    logger: Logger,
    queue_tx: channel::Sender<QueueItem>,
    abort_tx: channel::Sender<()>,
    abort_rx: channel::Receiver<()>,
    rak: Arc<RAK>,
    abort_batch: Arc<AtomicBool>,

    state: Mutex<Option<State>>,
    state_cond: Condvar,
}

impl Dispatcher {
    /// Create a new runtime call dispatcher.
    pub fn new(initializer: Box<dyn Initializer>, rak: Arc<RAK>) -> Arc<Self> {
        let (tx, rx) = channel::bounded(BACKLOG_SIZE);
        let (abort_tx, abort_rx) = channel::bounded(1);

        let dispatcher = Arc::new(Dispatcher {
            logger: get_logger("runtime/dispatcher"),
            queue_tx: tx,
            abort_tx: abort_tx,
            abort_rx: abort_rx,
            rak,
            abort_batch: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(None),
            state_cond: Condvar::new(),
        });

        let d = dispatcher.clone();
        thread::spawn(move || {
            let _guard = AbortOnPanic;
            d.run(initializer, rx)
        });

        dispatcher
    }

    /// Start the dispatcher.
    pub fn start(&self, protocol: Arc<Protocol>, consensus_verifier: Box<dyn Verifier>) {
        let mut s = self.state.lock().unwrap();
        *s = Some(State {
            protocol,
            consensus_verifier,
        });
        self.state_cond.notify_one();
    }

    /// Queue a new request to be dispatched.
    pub fn queue_request(&self, ctx: Context, id: u64, body: Body) -> AnyResult<()> {
        self.queue_tx.try_send((ctx, id, body))?;
        Ok(())
    }

    /// Signals to dispatcher that it should abort and waits for the abort to
    /// complete.
    pub fn abort_and_wait(&self, ctx: Context, id: u64, req: Body) -> AnyResult<()> {
        self.abort_batch.store(true, Ordering::SeqCst);
        // Queue the request to break the dispatch loop in case nothing is
        // being processed at the moment.
        self.queue_request(ctx, id, req)?;
        // Wait for abort.
        self.abort_rx.recv().map_err(|error| anyhow!("{}", error))
    }

    fn run(
        &self,
        initializer: Box<dyn Initializer>,
        rx: channel::Receiver<QueueItem>,
    ) -> AnyResult<()> {
        // Wait for the state to be available.
        let State {
            protocol,
            consensus_verifier,
        } = {
            let mut guard = self.state.lock().unwrap();
            while guard.is_none() {
                guard = self.state_cond.wait(guard).unwrap();
            }

            guard.take().unwrap()
        };

        // Create actual dispatchers for RPCs and transactions.
        info!(self.logger, "Starting the runtime dispatcher");
        let mut rpc_demux = RpcDemux::new(self.rak.clone());
        let mut rpc_dispatcher = RpcDispatcher::new();
        let mut txn_dispatcher: Box<dyn TxnDispatcher> = if let Some(txn) =
            initializer.init(&protocol, &self.rak, &mut rpc_demux, &mut rpc_dispatcher)
        {
            txn
        } else {
            Box::new(TxnNoopDispatcher::new())
        };
        txn_dispatcher.set_abort_batch_flag(self.abort_batch.clone());

        // Create the single-threaded Tokio runtime that can be used to schedule async I/O.
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        // Create common MKVS to use as a cache as long as the root stays the same. Use separate
        // caches for executing and checking transactions.
        let mut cache = Cache::new(protocol.clone());
        let mut cache_check = Cache::new(protocol.clone());

        'dispatch: loop {
            // Check if abort was requested and if so, signal that the batch
            // was aborted and reset the abort flag.
            if self
                .abort_batch
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.abort_tx.try_send(())?;
            }

            let (ctx, id, request) = match rx.recv() {
                Ok(data) => data,
                Err(error) => {
                    error!(self.logger, "Error while waiting for request"; "err" => %error);
                    break 'dispatch;
                }
            };

            let result = match request {
                Body::RuntimeRPCCallRequest { request } => {
                    // RPC call.
                    self.dispatch_rpc(
                        &mut rpc_demux,
                        &mut rpc_dispatcher,
                        &protocol,
                        &tokio_rt,
                        ctx,
                        request,
                    )
                }
                Body::RuntimeLocalRPCCallRequest { request } => {
                    // Local RPC call.
                    self.dispatch_local_rpc(&mut rpc_dispatcher, &protocol, &tokio_rt, ctx, request)
                }
                Body::RuntimeExecuteTxBatchRequest {
                    consensus_block,
                    round_results,
                    io_root,
                    inputs,
                    block,
                    epoch,
                    max_messages,
                } => {
                    // Transaction execution.
                    self.dispatch_txn(
                        ctx,
                        &mut cache,
                        &mut txn_dispatcher,
                        &protocol,
                        io_root,
                        inputs.unwrap_or_default(),
                        TxDispatchState {
                            tokio_rt: &tokio_rt,
                            consensus_block,
                            consensus_verifier: &consensus_verifier,
                            header: block.header,
                            epoch,
                            round_results,
                            max_messages,
                            check_only: false,
                        },
                    )
                }
                Body::RuntimeCheckTxBatchRequest {
                    consensus_block,
                    inputs,
                    block,
                    epoch,
                    max_messages,
                } => {
                    // Transaction check.
                    self.dispatch_txn(
                        ctx,
                        &mut cache_check,
                        &mut txn_dispatcher,
                        &protocol,
                        Hash::default(),
                        inputs,
                        TxDispatchState {
                            tokio_rt: &tokio_rt,
                            consensus_block,
                            consensus_verifier: &consensus_verifier,
                            header: block.header,
                            epoch,
                            round_results: Default::default(),
                            max_messages,
                            check_only: true,
                        },
                    )
                }
                Body::RuntimeKeyManagerPolicyUpdateRequest { signed_policy_raw } => {
                    // KeyManager policy update local RPC call.
                    self.handle_km_policy_update(&mut rpc_dispatcher, ctx, signed_policy_raw)
                }
                Body::RuntimeQueryRequest {
                    consensus_block,
                    header,
                    epoch,
                    max_messages,
                    method,
                    args,
                } => {
                    // Query.
                    self.dispatch_query(
                        ctx,
                        &mut cache_check,
                        &mut txn_dispatcher,
                        &protocol,
                        method,
                        args,
                        TxDispatchState {
                            tokio_rt: &tokio_rt,
                            consensus_block,
                            consensus_verifier: &consensus_verifier,
                            header,
                            epoch,
                            round_results: Default::default(),
                            max_messages,
                            check_only: true,
                        },
                    )
                }
                Body::RuntimeConsensusSyncRequest { height } => consensus_verifier
                    .sync(height)
                    .map_err(Into::into)
                    .map(|_| Body::RuntimeConsensusSyncResponse {}),
                Body::RuntimeAbortRequest {} => {
                    // We handle the RuntimeAbortRequest here so that we break
                    // the recv loop and re-check abort flag.
                    info!(self.logger, "Received abort request");
                    continue 'dispatch;
                }
                _ => {
                    error!(self.logger, "Unsupported request type");
                    break 'dispatch;
                }
            };

            let response = match result {
                Ok(body) => body,
                Err(error) => Body::Error(error),
            };
            protocol.send_response(id, response).unwrap();
        }

        info!(self.logger, "Runtime call dispatcher is terminating");

        Ok(())
    }

    fn dispatch_query(
        &self,
        ctx: Context,
        cache: &mut Cache,
        txn_dispatcher: &mut dyn TxnDispatcher,
        protocol: &Arc<Protocol>,
        method: String,
        args: cbor::Value,
        state: TxDispatchState,
    ) -> Result<Body, Error> {
        debug!(self.logger, "Received query request";
            "state_root" => ?state.header.state_root,
            "round" => ?state.header.round,
        );

        // Verify that the runtime ID matches the block's namespace. This is a protocol violation
        // as the compute node should never change the runtime ID.
        if state.header.namespace != protocol.get_runtime_id() {
            panic!(
                "block namespace does not match runtime id (namespace: {:?} runtime ID: {:?})",
                state.header.namespace,
                protocol.get_runtime_id(),
            );
        }

        // For queries we don't do any consensus layer integrity verification.
        let consensus_state = state
            .consensus_verifier
            .unverified_state(state.consensus_block)
            .expect("verification must succeed");

        // Create a new context and dispatch the batch.
        let ctx = ctx.freeze();
        cache.maybe_replace(Root {
            namespace: state.header.namespace,
            version: state.header.round,
            root_type: RootType::State,
            hash: state.header.state_root,
        });

        let mut overlay = OverlayTree::new(&mut cache.mkvs);
        let txn_ctx = TxnContext::new(
            ctx.clone(),
            state.tokio_rt,
            consensus_state,
            &mut overlay,
            &state.header,
            state.epoch,
            &state.round_results,
            state.max_messages,
            state.check_only,
        );
        txn_dispatcher
            .query(txn_ctx, &method, args)
            .map(|data| Body::RuntimeQueryResponse { data })
    }

    fn txn_check_batch(
        &self,
        ctx: Arc<Context>,
        cache: &mut Cache,
        txn_dispatcher: &mut dyn TxnDispatcher,
        inputs: TxnBatch,
        _io_root: Hash,
        state: TxDispatchState,
    ) -> Result<Body, Error> {
        // For check-only we don't do any consensus layer integrity verification.
        let consensus_state = state
            .consensus_verifier
            .unverified_state(state.consensus_block)?;

        let mut overlay = OverlayTree::new(&mut cache.mkvs);
        let txn_ctx = TxnContext::new(
            ctx.clone(),
            state.tokio_rt,
            consensus_state,
            &mut overlay,
            &state.header,
            state.epoch,
            &state.round_results,
            state.max_messages,
            state.check_only,
        );
        let results = txn_dispatcher.check_batch(txn_ctx, &inputs);

        debug!(self.logger, "Transaction batch check complete");

        results.map(|results| Body::RuntimeCheckTxBatchResponse { results })
    }

    fn txn_execute_batch(
        &self,
        ctx: Arc<Context>,
        cache: &mut Cache,
        txn_dispatcher: &mut dyn TxnDispatcher,
        mut inputs: TxnBatch,
        io_root: Hash,
        state: TxDispatchState,
    ) -> Result<Body, Error> {
        // Verify consensus state and runtime state root integrity before execution.
        let consensus_state = state
            .consensus_verifier
            .verify(state.consensus_block, state.header.clone())?;

        let header = &state.header;
        let mut overlay = OverlayTree::new(&mut cache.mkvs);
        let txn_ctx = TxnContext::new(
            ctx.clone(),
            state.tokio_rt,
            consensus_state,
            &mut overlay,
            header,
            state.epoch,
            &state.round_results,
            state.max_messages,
            state.check_only,
        );
        let mut results = txn_dispatcher.execute_batch(txn_ctx, &inputs)?;

        // Finalize state.
        let (state_write_log, new_state_root) = overlay
            .commit_both(
                Context::create_child(&ctx),
                header.namespace,
                header.round + 1,
            )
            .expect("state commit must succeed");

        txn_dispatcher.finalize(new_state_root);
        cache.commit(header.round + 1, new_state_root);

        // Generate I/O root. Since we already fetched the inputs we avoid the need
        // to fetch them again by generating the previous I/O tree (generated by the
        // transaction scheduler) from the inputs.
        let mut txn_tree = TxnTree::new(
            Box::new(NoopReadSyncer),
            Root {
                namespace: header.namespace,
                version: header.round + 1,
                root_type: RootType::IO,
                hash: Hash::empty_hash(),
            },
        );
        let mut hashes = Vec::new();
        for (batch_order, input) in inputs.drain(..).enumerate() {
            hashes.push(Hash::digest_bytes(&input));
            txn_tree
                .add_input(
                    Context::create_child(&ctx),
                    input,
                    batch_order.try_into().unwrap(),
                )
                .expect("add transaction must succeed");
        }

        let (_, old_io_root) = txn_tree
            .commit(Context::create_child(&ctx))
            .expect("io commit must succeed");
        if old_io_root != io_root {
            panic!(
                "dispatcher: I/O root inconsistent with inputs (expected: {:?} got: {:?})",
                io_root, old_io_root
            );
        }

        for (tx_hash, result) in hashes.drain(..).zip(results.results.drain(..)) {
            txn_tree
                .add_output(
                    Context::create_child(&ctx),
                    tx_hash,
                    result.output,
                    result.tags,
                )
                .expect("add transaction must succeed");
        }

        txn_tree
            .add_block_tags(Context::create_child(&ctx), results.block_tags)
            .expect("adding block tags must succeed");

        let (io_write_log, io_root) = txn_tree
            .commit(Context::create_child(&ctx))
            .expect("io commit must succeed");

        let header = ComputeResultsHeader {
            round: header.round + 1,
            previous_hash: header.encoded_hash(),
            io_root: Some(io_root),
            state_root: Some(new_state_root),
            messages_hash: Some(roothash::Message::messages_hash(&results.messages)),
        };

        // Since we've computed the batch, we can trust it.
        state
            .consensus_verifier
            .trust(&header)
            .expect("trusting a computed header must succeed");

        debug!(self.logger, "Transaction batch execution complete";
            "previous_hash" => ?header.previous_hash,
            "io_root" => ?header.io_root,
            "state_root" => ?header.state_root,
            "messages_hash" => ?header.messages_hash,
        );

        let rak_sig = if self.rak.public_key().is_some() {
            self.rak
                .sign(
                    &COMPUTE_RESULTS_HEADER_CONTEXT,
                    &cbor::to_vec(header.clone()),
                )
                .unwrap()
        } else {
            Signature::default()
        };

        Ok(Body::RuntimeExecuteTxBatchResponse {
            batch: ComputedBatch {
                header,
                io_write_log,
                state_write_log,
                rak_sig,
                messages: results.messages,
            },
            batch_weight_limits: results.batch_weight_limits,
        })
    }

    fn dispatch_txn(
        &self,
        ctx: Context,
        cache: &mut Cache,
        txn_dispatcher: &mut dyn TxnDispatcher,
        protocol: &Arc<Protocol>,
        io_root: Hash,
        inputs: TxnBatch,
        state: TxDispatchState,
    ) -> Result<Body, Error> {
        debug!(self.logger, "Received transaction batch request";
            "state_root" => ?state.header.state_root,
            "round" => state.header.round + 1,
            "round_results" => ?state.round_results,
            "check_only" => state.check_only,
        );

        // Verify that the runtime ID matches the block's namespace. This is a protocol violation
        // as the compute node should never change the runtime ID.
        if state.header.namespace != protocol.get_runtime_id() {
            panic!(
                "block namespace does not match runtime id (namespace: {:?} runtime ID: {:?})",
                state.header.namespace,
                protocol.get_runtime_id(),
            );
        }

        // Create a new context and dispatch the batch.
        let ctx = ctx.freeze();
        cache.maybe_replace(Root {
            namespace: state.header.namespace,
            version: state.header.round,
            root_type: RootType::State,
            hash: state.header.state_root,
        });

        if state.check_only {
            self.txn_check_batch(ctx, cache, txn_dispatcher, inputs, io_root, state)
        } else {
            self.txn_execute_batch(ctx, cache, txn_dispatcher, inputs, io_root, state)
        }
    }

    fn dispatch_rpc(
        &self,
        rpc_demux: &mut RpcDemux,
        rpc_dispatcher: &mut RpcDispatcher,
        protocol: &Arc<Protocol>,
        tokio_rt: &tokio::runtime::Runtime,
        ctx: Context,
        request: Vec<u8>,
    ) -> Result<Body, Error> {
        debug!(self.logger, "Received RPC call request");

        // Process frame.
        let mut buffer = vec![];
        let result = match rpc_demux.process_frame(request, &mut buffer) {
            Ok(result) => result,
            Err(error) => {
                error!(self.logger, "Error while processing frame"; "err" => %error);
                return Err(Error::new("rhp/dispatcher", 1, &format!("{}", error)));
            }
        };

        if let Some((session_id, session_info, message, untrusted_plaintext)) = result {
            // Dispatch request.
            assert!(
                buffer.is_empty(),
                "must have no handshake data in transport mode"
            );

            match message {
                RpcMessage::Request(req) => {
                    // First make sure that the untrusted_plaintext matches
                    // the request's method!
                    if untrusted_plaintext != req.method {
                        error!(self.logger, "Request methods don't match!";
                            "untrusted_plaintext" => ?untrusted_plaintext,
                            "method" => ?req.method
                        );
                        return Err(Error::new(
                            "rhp/dispatcher",
                            1,
                            "Request's method doesn't match untrusted_plaintext copy.",
                        ));
                    }

                    // Request, dispatch.
                    let ctx = ctx.freeze();
                    let untrusted_local = Arc::new(ProtocolUntrustedLocalStorage::new(
                        Context::create_child(&ctx),
                        protocol.clone(),
                    ));
                    let rpc_ctx = RpcContext::new(
                        ctx.clone(),
                        tokio_rt,
                        self.rak.clone(),
                        session_info,
                        &untrusted_local,
                    );
                    let response = rpc_dispatcher.dispatch(req, rpc_ctx);
                    let response = RpcMessage::Response(response);

                    // Note: MKVS commit is omitted, this MUST be global side-effect free.

                    debug!(self.logger, "RPC call dispatch complete");

                    let mut buffer = vec![];
                    match rpc_demux.write_message(session_id, response, &mut buffer) {
                        Ok(_) => {
                            // Transmit response.
                            Ok(Body::RuntimeRPCCallResponse { response: buffer })
                        }
                        Err(error) => {
                            error!(self.logger, "Error while writing response"; "err" => %error);
                            Err(Error::new("rhp/dispatcher", 1, &format!("{}", error)))
                        }
                    }
                }
                RpcMessage::Close => {
                    // Session close.
                    let mut buffer = vec![];
                    match rpc_demux.close(session_id, &mut buffer) {
                        Ok(_) => {
                            // Transmit response.
                            Ok(Body::RuntimeRPCCallResponse { response: buffer })
                        }
                        Err(error) => {
                            error!(self.logger, "Error while closing session"; "err" => %error);
                            Err(Error::new("rhp/dispatcher", 1, &format!("{}", error)))
                        }
                    }
                }
                msg => {
                    warn!(self.logger, "Ignoring invalid RPC message type"; "msg" => ?msg);
                    Err(Error::new("rhp/dispatcher", 1, "invalid RPC message type"))
                }
            }
        } else {
            // Send back any handshake frames.
            Ok(Body::RuntimeRPCCallResponse { response: buffer })
        }
    }

    fn dispatch_local_rpc(
        &self,
        rpc_dispatcher: &mut RpcDispatcher,
        protocol: &Arc<Protocol>,
        tokio_rt: &tokio::runtime::Runtime,
        ctx: Context,
        request: Vec<u8>,
    ) -> Result<Body, Error> {
        debug!(self.logger, "Received local RPC call request");

        let req: RpcRequest = cbor::from_slice(&request)
            .map_err(|_| Error::new("rhp/dispatcher", 1, "malformed request"))?;

        // Request, dispatch.
        let ctx = ctx.freeze();
        let untrusted_local = Arc::new(ProtocolUntrustedLocalStorage::new(
            Context::create_child(&ctx),
            protocol.clone(),
        ));
        let rpc_ctx = RpcContext::new(
            ctx.clone(),
            tokio_rt,
            self.rak.clone(),
            None,
            &untrusted_local,
        );
        let response = rpc_dispatcher.dispatch_local(req, rpc_ctx);
        let response = RpcMessage::Response(response);

        // Note: MKVS commit is omitted, this MUST be global side-effect free.

        debug!(self.logger, "Local RPC call dispatch complete");

        let response = cbor::to_vec(response);
        Ok(Body::RuntimeLocalRPCCallResponse { response })
    }

    fn handle_km_policy_update(
        &self,
        rpc_dispatcher: &mut RpcDispatcher,
        _ctx: Context,
        signed_policy_raw: Vec<u8>,
    ) -> Result<Body, Error> {
        debug!(self.logger, "Received km policy update request");
        rpc_dispatcher.handle_km_policy_update(signed_policy_raw);
        debug!(self.logger, "KM policy update request complete");

        Ok(Body::RuntimeKeyManagerPolicyUpdateResponse {})
    }
}

struct Cache {
    protocol: Arc<Protocol>,
    mkvs: Tree,
    root: Root,
}

impl Cache {
    fn new(protocol: Arc<Protocol>) -> Self {
        Self {
            mkvs: Self::new_tree(&protocol, Default::default()),
            root: Default::default(),
            protocol,
        }
    }

    fn new_tree(protocol: &Arc<Protocol>, root: Root) -> Tree {
        let read_syncer = HostReadSyncer::new(protocol.clone(), HostStorageEndpoint::Runtime);
        Tree::make()
            .with_capacity(100_000, 10_000_000)
            .with_root(root)
            .new(Box::new(read_syncer))
    }

    fn maybe_replace(&mut self, root: Root) {
        if self.root == root {
            return;
        }

        self.mkvs = Self::new_tree(&self.protocol, root);
        self.root = root;
    }

    fn commit(&mut self, version: u64, root_hash: Hash) {
        self.root.version = version;
        self.root.hash = root_hash;
    }
}
