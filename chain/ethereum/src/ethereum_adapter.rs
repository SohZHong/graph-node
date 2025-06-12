use alloy::primitives::{TxKind, B256, B64};
use alloy::providers::{Provider, ProviderBuilder};
use alloy_rpc_types::{
    BlockTransactions, FilterBlockOption, TransactionInput, TransactionRequest, TransactionTrait,
};
use futures03::{future::BoxFuture, stream::FuturesUnordered};
use graph::abi;
use graph::abi::DynSolValueExt;
use graph::abi::FunctionExt;
use graph::blockchain::client::ChainClient;
use graph::blockchain::BlockHash;
use graph::blockchain::ChainIdentifier;
use graph::blockchain::ExtendedBlockPtr;
use graph::components::transaction_receipt::LightTransactionReceipt;
use graph::data::store::ethereum::call;
use graph::data::store::scalar;
use graph::data::subgraph::UnifiedMappingApiVersion;
use graph::data::subgraph::API_VERSION_0_0_7;
use graph::data_source::common::ContractCall;
use graph::futures01::stream;
use graph::futures01::Future;
use graph::futures01::Stream;
use graph::futures03::future::try_join_all;
use graph::futures03::{
    self, compat::Future01CompatExt, FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use graph::prelude::tokio::try_join;
use graph::prelude::web3::types::{H2048, H64, U256};
use graph::slog::o;
use graph::tokio::sync::RwLock;
use graph::tokio::time::timeout;
use graph::{
    blockchain::{block_stream::BlockWithTriggers, BlockPtr, IngestorError},
    prelude::{
        anyhow::{self, anyhow, bail, ensure, Context},
        async_trait, debug, error, hex, info, retry, serde_json as json, tiny_keccak, trace, warn,
        web3::{
            self,
            types::{
                Address, BlockId, BlockNumber as Web3BlockNumber, Bytes, CallRequest, Filter,
                FilterBuilder, Log, Transaction, TransactionReceipt, H256,
            },
        },
        BlockNumber, ChainStore, CheapClone, DynTryFuture, Error, EthereumCallCache, Logger,
        TimeoutError,
    },
};
use graph::{
    components::ethereum::*,
    prelude::web3::api::Web3,
    prelude::web3::transports::Batch,
    prelude::web3::types::{Trace, TraceFilter, TraceFilterBuilder, H160},
};
use itertools::Itertools;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryFrom;
use std::fmt::Formatter;
use std::iter::FromIterator;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use crate::adapter::EthereumRpcError;
use crate::adapter::ProviderStatus;
use crate::chain::BlockFinality;
use crate::trigger::LogRef;
use crate::Chain;
use crate::NodeCapabilities;
use crate::TriggerFilter;
use crate::{
    adapter::{
        ContractCallError, EthGetLogsFilter, EthereumAdapter as EthereumAdapterTrait,
        EthereumBlockFilter, EthereumCallFilter, EthereumLogFilter, ProviderEthRpcMetrics,
        SubgraphEthRpcMetrics,
    },
    transport::Transport,
    trigger::{EthereumBlockTriggerType, EthereumTrigger},
    ENV_VARS,
};

#[derive(Clone)]
pub struct EthereumAdapter {
    logger: Logger,
    provider: String,
    web3: Arc<Web3<Transport>>,
    alloy: Arc<dyn Provider>,
    metrics: Arc<ProviderEthRpcMetrics>,
    supports_eip_1898: bool,
    call_only: bool,
    supports_block_receipts: Arc<RwLock<Option<bool>>>,
}

// TODO: remove this hacky implementation
impl std::fmt::Debug for EthereumAdapter {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "EthereumAdapter")
    }
}

impl CheapClone for EthereumAdapter {
    fn cheap_clone(&self) -> Self {
        Self {
            logger: self.logger.clone(),
            provider: self.provider.clone(),
            web3: self.web3.cheap_clone(),
            alloy: self.alloy.clone(),
            metrics: self.metrics.cheap_clone(),
            supports_eip_1898: self.supports_eip_1898,
            call_only: self.call_only,
            supports_block_receipts: self.supports_block_receipts.cheap_clone(),
        }
    }
}

impl EthereumAdapter {
    pub fn is_call_only(&self) -> bool {
        self.call_only
    }

    pub async fn new(
        logger: Logger,
        provider: String,
        transport: Transport,
        provider_metrics: Arc<ProviderEthRpcMetrics>,
        supports_eip_1898: bool,
        call_only: bool,
    ) -> Self {
        let rpc_url = match &transport {
            Transport::RPC {
                client: _,
                metrics: _,
                provider: _,
                rpc_url,
            } => rpc_url.clone(),
            Transport::IPC(_ipc) => todo!(),
            Transport::WS(_web_socket) => todo!(),
        };
        let web3 = Arc::new(Web3::new(transport));
        let alloy = Arc::new(ProviderBuilder::new().connect(&rpc_url).await.unwrap());

        EthereumAdapter {
            logger,
            provider,
            web3,
            alloy,
            metrics: provider_metrics,
            supports_eip_1898,
            call_only,
            supports_block_receipts: Arc::new(RwLock::new(None)),
        }
    }

    async fn traces(
        self,
        logger: Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        from: BlockNumber,
        to: BlockNumber,
        addresses: Vec<H160>,
    ) -> Result<Vec<Trace>, Error> {
        info!(logger, "!!!! traces");
        assert!(!self.call_only);

        let eth = self.clone();
        let retry_log_message =
            format!("trace_filter RPC call for block range: [{}..{}]", from, to);
        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let trace_filter: TraceFilter = match addresses.len() {
                    0 => TraceFilterBuilder::default()
                        .from_block(from.into())
                        .to_block(to.into())
                        .build(),
                    _ => TraceFilterBuilder::default()
                        .from_block(from.into())
                        .to_block(to.into())
                        .to_address(addresses.clone())
                        .build(),
                };

                let eth = eth.cheap_clone();
                let logger_for_triggers = logger.clone();
                let logger_for_error = logger.clone();
                let start = Instant::now();
                let subgraph_metrics = subgraph_metrics.clone();
                let provider_metrics = eth.metrics.clone();
                let provider = self.provider.clone();

                async move {
                    let result = eth
                        .web3
                        .trace()
                        .filter(trace_filter)
                        .await
                        .map(move |traces| {
                            if !traces.is_empty() {
                                if to == from {
                                    debug!(
                                        logger_for_triggers,
                                        "Received {} traces for block {}",
                                        traces.len(),
                                        to
                                    );
                                } else {
                                    debug!(
                                        logger_for_triggers,
                                        "Received {} traces for blocks [{}, {}]",
                                        traces.len(),
                                        from,
                                        to
                                    );
                                }
                            }
                            traces
                        })
                        .map_err(Error::from);

                    let elapsed = start.elapsed().as_secs_f64();
                    provider_metrics.observe_request(elapsed, "trace_filter", &provider);
                    subgraph_metrics.observe_request(elapsed, "trace_filter", &provider);
                    if let Err(e) = &result {
                        provider_metrics.add_error("trace_filter", &provider);
                        subgraph_metrics.add_error("trace_filter", &provider);
                        debug!(
                            logger_for_error,
                            "Error querying traces error = {:#} from = {} to = {}", e, from, to
                        );
                    }
                    result
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow::anyhow!(
                        "Ethereum node took too long to respond to trace_filter \
                         (from block {}, to block {})",
                        from,
                        to
                    )
                })
            })
            .await
    }

    // This is a lazy check for block receipt support. It is only called once and then the result is
    // cached. The result is not used for anything critical, so it is fine to be lazy.
    async fn check_block_receipt_support_and_update_cache(
        &self,
        alloy: Arc<dyn Provider + 'static>,
        web3: Arc<Web3<Transport>>,
        block_hash: H256,
        supports_eip_1898: bool,
        call_only: bool,
        logger: Logger,
    ) -> bool {
        // This is the lazy part. If the result is already in `supports_block_receipts`, we don't need
        // to check again.
        {
            let supports_block_receipts = self.supports_block_receipts.read().await;
            if let Some(supports_block_receipts) = *supports_block_receipts {
                return supports_block_receipts;
            }
        }

        info!(logger, "Checking eth_getBlockReceipts support");
        let result = timeout(
            ENV_VARS.block_receipts_check_timeout,
            check_block_receipt_support(alloy, web3, block_hash, supports_eip_1898, call_only),
        )
        .await;

        let result = match result {
            Ok(Ok(_)) => {
                info!(logger, "Provider supports block receipts");
                true
            }
            Ok(Err(err)) => {
                warn!(logger, "Skipping use of block receipts, reason: {}", err);
                false
            }
            Err(_) => {
                warn!(
                    logger,
                    "Skipping use of block receipts, reason: Timeout after {} seconds",
                    ENV_VARS.block_receipts_check_timeout.as_secs()
                );
                false
            }
        };

        // We set the result in `self.supports_block_receipts` so that the next time this function is called, we don't
        // need to check again.
        let mut supports_block_receipts = self.supports_block_receipts.write().await;
        if supports_block_receipts.is_none() {
            *supports_block_receipts = Some(result);
        }

        result
    }

    async fn logs_with_sigs(
        &self,
        logger: Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        from: BlockNumber,
        to: BlockNumber,
        filter: Arc<EthGetLogsFilter>,
        too_many_logs_fingerprints: &'static [&'static str],
    ) -> Result<Vec<Log>, TimeoutError<web3::error::Error>> {
        assert!(!self.call_only);

        let eth_adapter = self.clone();
        let retry_log_message = format!("eth_getLogs RPC call for block range: [{}..{}]", from, to);
        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .when(move |res: &Result<_, web3::error::Error>| match res {
                Ok(_) => false,
                Err(e) => !too_many_logs_fingerprints
                    .iter()
                    .any(|f| e.to_string().contains(f)),
            })
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let eth_adapter = eth_adapter.cheap_clone();
                let subgraph_metrics = subgraph_metrics.clone();
                let provider_metrics = eth_adapter.metrics.clone();
                let filter = filter.clone();
                let provider = eth_adapter.provider.clone();

                async move {
                    let start = Instant::now();
                    let block_option = FilterBlockOption::default()
                        .with_from_block((from as u64).into())
                        .with_to_block((to as u64).into());
                    let address: alloy_rpc_types::FilterSet<alloy::primitives::Address> = filter
                        .contracts
                        .iter()
                        .map(|c| h160_to_address(c))
                        .collect();
                    let topic0 = convert_topic(&Some(filter.event_signatures.clone()));
                    let topic1 = convert_topic(&filter.topic1);
                    let topic2 = convert_topic(&filter.topic2);
                    let topic3 = convert_topic(&filter.topic3);
                    let topics = [topic0, topic1, topic2, topic3];
                    let filter2 = alloy_rpc_types::Filter {
                        block_option,
                        address,
                        topics,
                    };
                    let result1 = eth_adapter.alloy.get_logs(&filter2).await.unwrap();
                    let result2 = convert_log(&result1);

                    // Create a log filter
                    let log_filter: Filter = FilterBuilder::default()
                        .from_block(from.into())
                        .to_block(to.into())
                        .address(filter.contracts.clone())
                        .topics(
                            Some(filter.event_signatures.clone()),
                            filter.topic1.clone(),
                            filter.topic2.clone(),
                            filter.topic3.clone(),
                        )
                        .build();

                    // Request logs from client
                    let result3 = eth_adapter.web3.eth().logs(log_filter).boxed().await;
                    match &result3 {
                        Ok(res) => assert_eq!(&result2, res),
                        Err(_) => {}
                    }
                    // assert_eq!(Ok(result2), result3);
                    let elapsed = start.elapsed().as_secs_f64();
                    provider_metrics.observe_request(elapsed, "eth_getLogs", &provider);
                    subgraph_metrics.observe_request(elapsed, "eth_getLogs", &provider);
                    if result3.is_err() {
                        provider_metrics.add_error("eth_getLogs", &provider);
                        subgraph_metrics.add_error("eth_getLogs", &provider);
                    }
                    Ok(result2)
                }
            })
            .await
    }

    fn trace_stream(
        self,
        logger: &Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        from: BlockNumber,
        to: BlockNumber,
        addresses: Vec<H160>,
    ) -> impl Stream<Item = Trace, Error = Error> + Send {
        if from > to {
            panic!(
                "Can not produce a call stream on a backwards block range: from = {}, to = {}",
                from, to,
            );
        }

        // Go one block at a time if requesting all traces, to not overload the RPC.
        let step_size = match addresses.is_empty() {
            false => ENV_VARS.trace_stream_step_size,
            true => 1,
        };

        let eth = self;
        let logger = logger.clone();
        stream::unfold(from, move |start| {
            if start > to {
                return None;
            }
            let end = (start + step_size - 1).min(to);
            let new_start = end + 1;
            if start == end {
                debug!(logger, "Requesting traces for block {}", start);
            } else {
                debug!(logger, "Requesting traces for blocks [{}, {}]", start, end);
            }
            Some(graph::futures01::future::ok((
                eth.clone()
                    .traces(
                        logger.cheap_clone(),
                        subgraph_metrics.clone(),
                        start,
                        end,
                        addresses.clone(),
                    )
                    .boxed()
                    .compat(),
                new_start,
            )))
        })
        .buffered(ENV_VARS.block_batch_size)
        .map(stream::iter_ok)
        .flatten()
    }

    fn log_stream(
        &self,
        logger: Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        from: BlockNumber,
        to: BlockNumber,
        filter: EthGetLogsFilter,
    ) -> DynTryFuture<'static, Vec<Log>, Error> {
        // Codes returned by Ethereum node providers if an eth_getLogs request is too heavy.
        const TOO_MANY_LOGS_FINGERPRINTS: &[&str] = &[
            "ServerError(-32005)",       // Infura
            "503 Service Unavailable",   // Alchemy
            "ServerError(-32000)",       // Alchemy
            "Try with this block range", // zKSync era
            "block range too large",     // Monad
        ];

        if from > to {
            panic!(
                "cannot produce a log stream on a backwards block range (from={}, to={})",
                from, to
            );
        }

        // Collect all event sigs
        let eth = self.cheap_clone();
        let filter = Arc::new(filter);

        let step = match filter.contracts.is_empty() {
            // `to - from + 1`  blocks will be scanned.
            false => to - from,
            true => (to - from).min(ENV_VARS.max_event_only_range - 1),
        };

        // Typically this will loop only once and fetch the entire range in one request. But if the
        // node returns an error that signifies the request is to heavy to process, the range will
        // be broken down to smaller steps.
        futures03::stream::try_unfold((from, step), move |(start, step)| {
            let logger = logger.cheap_clone();
            let filter = filter.cheap_clone();
            let eth = eth.cheap_clone();
            let subgraph_metrics = subgraph_metrics.cheap_clone();

            async move {
                if start > to {
                    return Ok(None);
                }

                let end = (start + step).min(to);
                debug!(
                    logger,
                    "Requesting logs for blocks [{}, {}], {}", start, end, filter
                );
                let res = eth
                    .logs_with_sigs(
                        logger.cheap_clone(),
                        subgraph_metrics.cheap_clone(),
                        start,
                        end,
                        filter.cheap_clone(),
                        TOO_MANY_LOGS_FINGERPRINTS,
                    )
                    .await;

                match res {
                    Err(e) => {
                        let string_err = e.to_string();

                        // If the step is already 0, the request is too heavy even for a single
                        // block. We hope this never happens, but if it does, make sure to error.
                        if TOO_MANY_LOGS_FINGERPRINTS
                            .iter()
                            .any(|f| string_err.contains(f))
                            && step > 0
                        {
                            // The range size for a request is `step + 1`. So it's ok if the step
                            // goes down to 0, in that case we'll request one block at a time.
                            let new_step = step / 10;
                            debug!(logger, "Reducing block range size to scan for events";
                                               "new_size" => new_step + 1);
                            Ok(Some((vec![], (start, new_step))))
                        } else {
                            warn!(logger, "Unexpected RPC error"; "error" => &string_err);
                            Err(anyhow!("{}", string_err))
                        }
                    }
                    Ok(logs) => Ok(Some((logs, (end + 1, step)))),
                }
            }
        })
        .try_concat()
        .boxed()
    }

    // Method to determine block_id based on support for EIP-1898
    fn block_ptr_to_id(&self, block_ptr: &BlockPtr) -> BlockId {
        // Ganache does not support calls by block hash.
        // See https://github.com/trufflesuite/ganache-cli/issues/973
        if !self.supports_eip_1898 {
            BlockId::Number(block_ptr.number.into())
        } else {
            BlockId::Hash(block_ptr.hash_as_h256())
        }
    }

    // Method to determine block_id based on support for EIP-1898
    fn block_ptr_to_id2(&self, block_ptr: &BlockPtr) -> alloy_rpc_types::BlockId {
        // Ganache does not support calls by block hash.
        // See https://github.com/trufflesuite/ganache-cli/issues/973
        if !self.supports_eip_1898 {
            alloy_rpc_types::BlockId::number(block_ptr.number as u64)
        } else {
            alloy_rpc_types::BlockId::hash(h256_to_b256(&block_ptr.hash_as_h256()))
        }
    }

    async fn code(
        &self,
        logger: &Logger,
        address: Address,
        block_ptr: BlockPtr,
    ) -> Result<Bytes, EthereumRpcError> {
        let web3 = self.web3.clone();
        let alloy = self.alloy.clone();
        let logger = Logger::new(&logger, o!("provider" => self.provider.clone()));

        let block_id = self.block_ptr_to_id(&block_ptr);
        let block_id2 = self.block_ptr_to_id2(&block_ptr);
        let address2 = h160_to_address(&address);
        let retry_log_message = format!("eth_getCode RPC call for block {}", block_ptr);

        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .when(|result| match result {
                Ok(_) => false,
                Err(_) => true,
            })
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                let alloy = alloy.clone();
                async move {
                    let result: Result<Bytes, web3::Error> =
                        web3.eth().code(address, Some(block_id)).boxed().await;
                    let result2 = alloy
                        .get_code_at(address2)
                        .block_id(block_id2)
                        .await
                        .map(bytes_to_bytes);
                    match result {
                        Ok(code) => {
                            assert_eq!(result2.unwrap(), code);
                            Ok(code)
                        }
                        Err(err) => Err(EthereumRpcError::Web3Error(err)),
                    }
                }
            })
            .await
            .map_err(|e| e.into_inner().unwrap_or(EthereumRpcError::Timeout))
    }

    async fn balance(
        &self,
        logger: &Logger,
        address: Address,
        block_ptr: BlockPtr,
    ) -> Result<U256, EthereumRpcError> {
        let web3 = self.web3.clone();
        let alloy = self.alloy.clone();
        let logger = Logger::new(&logger, o!("provider" => self.provider.clone()));

        let block_id = self.block_ptr_to_id(&block_ptr);
        let block_id2 = self.block_ptr_to_id2(&block_ptr);
        let address2 = h160_to_address(&address);
        let retry_log_message = format!("eth_getBalance RPC call for block {}", block_ptr);

        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .when(|result| match result {
                Ok(_) => false,
                Err(_) => true,
            })
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                let alloy = alloy.clone();
                async move {
                    let result: Result<U256, web3::Error> =
                        web3.eth().balance(address, Some(block_id)).boxed().await;
                    let result2 = alloy
                        .get_balance(address2)
                        .block_id(block_id2)
                        .await
                        .map(u256_to_u256);
                    match result {
                        Ok(balance) => {
                            assert_eq!(result2.unwrap(), balance);
                            Ok(balance)
                        }
                        Err(err) => Err(EthereumRpcError::Web3Error(err)),
                    }
                }
            })
            .await
            .map_err(|e| e.into_inner().unwrap_or(EthereumRpcError::Timeout))
    }

    async fn call(
        &self,
        logger: Logger,
        call_data: call::Request,
        block_ptr: BlockPtr,
        gas: Option<u32>,
    ) -> Result<call::Retval, ContractCallError> {
        fn reverted(logger: &Logger, reason: &str) -> Result<call::Retval, ContractCallError> {
            info!(logger, "Contract call reverted"; "reason" => reason);
            Ok(call::Retval::Null)
        }

        let web3 = self.web3.clone();
        let alloy = self.alloy.clone();
        let logger = Logger::new(&logger, o!("provider" => self.provider.clone()));

        let block_id = self.block_ptr_to_id(&block_ptr);
        let block_id2 = self.block_ptr_to_id2(&block_ptr);
        let retry_log_message = format!("eth_call RPC call for block {}", block_ptr);
        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let call_data = call_data.clone();
                let web3 = web3.cheap_clone();
                let alloy = alloy.clone();
                let logger = logger.cheap_clone();
                async move {
                    let req = CallRequest {
                        to: Some(call_data.address),
                        gas: gas.map(|val| web3::types::U256::from(val)),
                        data: Some(Bytes::from(call_data.encoded_call.to_vec())),
                        from: None,
                        gas_price: None,
                        value: None,
                        access_list: None,
                        max_fee_per_gas: None,
                        max_priority_fee_per_gas: None,
                        transaction_type: None,
                    };
                    let result = web3.eth().call(req, Some(block_id)).boxed().await;
                    let gas = gas.map(|val| val as u64);
                    let to = Some(TxKind::from(h160_to_address(&call_data.address)));
                    let input = TransactionInput {
                        input: None,
                        data: Some(alloy::primitives::Bytes::from(
                            call_data.encoded_call.to_vec(),
                        )),
                    };
                    let tx_req = TransactionRequest {
                        from: None,
                        to,
                        gas_price: None,
                        max_fee_per_gas: None,
                        max_priority_fee_per_gas: None,
                        max_fee_per_blob_gas: None,
                        gas,
                        value: None,
                        input,
                        nonce: None,
                        chain_id: None,
                        access_list: None,
                        transaction_type: None,
                        blob_versioned_hashes: None,
                        sidecar: None,
                        authorization_list: None,
                    };
                    let result2 = alloy
                        .call(tx_req)
                        .block(block_id2)
                        .await
                        .map(bytes_to_bytes);

                    // Try to check if the call was reverted. The JSON-RPC response for reverts is
                    // not standardized, so we have ad-hoc checks for each Ethereum client.

                    // 0xfe is the "designated bad instruction" of the EVM, and Solidity uses it for
                    // asserts.
                    const PARITY_BAD_INSTRUCTION_FE: &str = "Bad instruction fe";

                    // 0xfd is REVERT, but on some contracts, and only on older blocks,
                    // this happens. Makes sense to consider it a revert as well.
                    const PARITY_BAD_INSTRUCTION_FD: &str = "Bad instruction fd";

                    const PARITY_BAD_JUMP_PREFIX: &str = "Bad jump";
                    const PARITY_STACK_LIMIT_PREFIX: &str = "Out of stack";

                    // See f0af4ab0-6b7c-4b68-9141-5b79346a5f61.
                    const PARITY_OUT_OF_GAS: &str = "Out of gas";

                    // Also covers Nethermind reverts
                    const PARITY_VM_EXECUTION_ERROR: i64 = -32015;
                    const PARITY_REVERT_PREFIX: &str = "revert";

                    const XDAI_REVERT: &str = "revert";

                    // Deterministic Geth execution errors. We might need to expand this as
                    // subgraphs come across other errors. See
                    // https://github.com/ethereum/go-ethereum/blob/cd57d5cd38ef692de8fbedaa56598b4e9fbfbabc/core/vm/errors.go
                    const GETH_EXECUTION_ERRORS: &[&str] = &[
                        // The "revert" substring covers a few known error messages, including:
                        // Hardhat: "error: transaction reverted",
                        // Ganache and Moonbeam: "vm exception while processing transaction: revert",
                        // Geth: "execution reverted"
                        // And others.
                        "revert",
                        "invalid jump destination",
                        "invalid opcode",
                        // Ethereum says 1024 is the stack sizes limit, so this is deterministic.
                        "stack limit reached 1024",
                        // See f0af4ab0-6b7c-4b68-9141-5b79346a5f61 for why the gas limit is considered deterministic.
                        "out of gas",
                        "stack underflow",
                    ];

                    let env_geth_call_errors = ENV_VARS.geth_eth_call_errors.iter();
                    let mut geth_execution_errors = GETH_EXECUTION_ERRORS
                        .iter()
                        .copied()
                        .chain(env_geth_call_errors.map(|s| s.as_str()));

                    let as_solidity_revert_with_reason = |bytes: &[u8]| {
                        let solidity_revert_function_selector =
                            &tiny_keccak::keccak256(b"Error(string)")[..4];

                        match bytes.len() >= 4 && &bytes[..4] == solidity_revert_function_selector {
                            false => None,
                            true => abi::DynSolType::String
                                .abi_decode(&bytes[4..])
                                .ok()
                                .and_then(|val| val.clone().as_str().map(ToOwned::to_owned)),
                        }
                    };

                    match result {
                        // A successful response.
                        Ok(bytes) => {
                            assert_eq!(result2.unwrap(), bytes);
                            Ok(call::Retval::Value(scalar::Bytes::from(bytes)))
                        }

                        // Check for Geth revert.
                        Err(web3::Error::Rpc(rpc_error))
                            if geth_execution_errors
                                .any(|e| rpc_error.message.to_lowercase().contains(e)) =>
                        {
                            reverted(&logger, &rpc_error.message)
                        }

                        // Check for Parity revert.
                        Err(web3::Error::Rpc(ref rpc_error))
                            if rpc_error.code.code() == PARITY_VM_EXECUTION_ERROR =>
                        {
                            match rpc_error.data.as_ref().and_then(|d| d.as_str()) {
                                Some(data)
                                    if data.to_lowercase().starts_with(PARITY_REVERT_PREFIX)
                                        || data.starts_with(PARITY_BAD_JUMP_PREFIX)
                                        || data.starts_with(PARITY_STACK_LIMIT_PREFIX)
                                        || data == PARITY_BAD_INSTRUCTION_FE
                                        || data == PARITY_BAD_INSTRUCTION_FD
                                        || data == PARITY_OUT_OF_GAS
                                        || data == XDAI_REVERT =>
                                {
                                    let reason = if data == PARITY_BAD_INSTRUCTION_FE {
                                        PARITY_BAD_INSTRUCTION_FE.to_owned()
                                    } else {
                                        let payload = data.trim_start_matches(PARITY_REVERT_PREFIX);
                                        hex::decode(payload)
                                            .ok()
                                            .and_then(|payload| {
                                                as_solidity_revert_with_reason(&payload)
                                            })
                                            .unwrap_or("no reason".to_owned())
                                    };
                                    reverted(&logger, &reason)
                                }

                                // The VM execution error was not identified as a revert.
                                _ => Err(ContractCallError::Web3Error(web3::Error::Rpc(
                                    rpc_error.clone(),
                                ))),
                            }
                        }

                        // The error was not identified as a revert.
                        Err(err) => Err(ContractCallError::Web3Error(err)),
                    }
                }
            })
            .map_err(|e| e.into_inner().unwrap_or(ContractCallError::Timeout))
            .boxed()
            .await
    }

    async fn call_and_cache(
        &self,
        logger: &Logger,
        call: &ContractCall,
        req: call::Request,
        cache: Arc<dyn EthereumCallCache>,
    ) -> Result<call::Response, ContractCallError> {
        let result = self
            .call(
                logger.clone(),
                req.cheap_clone(),
                call.block_ptr.clone(),
                call.gas,
            )
            .await?;
        let _ = cache
            .set_call(
                &logger,
                req.cheap_clone(),
                call.block_ptr.cheap_clone(),
                result.clone(),
            )
            .map_err(|e| {
                error!(logger, "EthereumAdapter: call cache set error";
                        "contract_address" => format!("{:?}", req.address),
                        "error" => e.to_string())
            });

        Ok(req.response(result, call::Source::Rpc))
    }

    async fn load_latest_block_rpc_alloy(
        alloy: Arc<dyn Provider>,
        logger: &Logger,
    ) -> Result<Option<Arc<web3::types::Block<H256>>>, anyhow::Error> {
        let latest_block = alloy.get_block_number().await?;
        Self::load_block_rpc_alloy(alloy, latest_block, logger).await
    }

    async fn load_block_rpc_alloy(
        alloy: Arc<dyn Provider>,
        block_number: u64,
        logger: &Logger,
    ) -> Result<Option<Arc<web3::types::Block<H256>>>, anyhow::Error> {
        let number = alloy_rpc_types::BlockId::number(block_number);
        let block = alloy
            .get_block(number)
            .await?
            .map(|block| convert_block_hash_alloy2web3(&logger, block));
        Ok(block)
    }

    async fn load_full_block_rpc_alloy(
        alloy: Arc<dyn Provider>,
        logger: Logger,
        id: H256,
    ) -> Result<Arc<LightEthereumBlock>, anyhow::Error> {
        let hash: alloy_rpc_types::BlockId =
            alloy_rpc_types::BlockId::hash(B256::new(*id.as_fixed_bytes()));
        let block = alloy.get_block(hash).full().await.unwrap();
        if let Some(block) = block {
            Ok(convert_block_alloy2web3(&logger, block))
        } else {
            Err(anyhow!("Ethereum node did not find block {:?}", hash))
        }
    }

    fn load_blocks_rpc_alloy(
        &self,
        logger: Logger,
        ids: Vec<H256>,
    ) -> impl Stream<Item = Arc<LightEthereumBlock>, Error = Error> + Send {
        let alloy = self.alloy.clone();
        let logger = logger.clone();

        stream::iter_ok::<_, Error>(ids.into_iter().map(move |hash| {
            let alloy = alloy.clone();
            let logger = logger.clone();

            retry(format!("load block {}", hash), &logger)
                .redact_log_urls(true)
                .limit(ENV_VARS.request_retries)
                .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                .run(move || Self::load_full_block_rpc_alloy(alloy.clone(), logger.clone(), hash))
                .boxed()
                .compat()
                .from_err()
        }))
        .buffered(ENV_VARS.block_batch_size)
    }

    /// Request blocks by hash through JSON-RPC.
    fn load_blocks_rpc(
        &self,
        logger: Logger,
        ids: Vec<H256>,
    ) -> impl Stream<Item = Arc<LightEthereumBlock>, Error = Error> + Send {
        let web3 = self.web3.clone();

        stream::iter_ok::<_, Error>(ids.into_iter().map(move |hash| {
            let web3 = web3.clone();
            retry(format!("load block {}", hash), &logger)
                .redact_log_urls(true)
                .limit(ENV_VARS.request_retries)
                .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                .run(move || {
                    Box::pin(web3.eth().block_with_txs(BlockId::Hash(hash)))
                        .compat()
                        .from_err::<Error>()
                        .and_then(move |block| {
                            block.map(Arc::new).ok_or_else(|| {
                                anyhow::anyhow!("Ethereum node did not find block {:?}", hash)
                            })
                        })
                        .compat()
                })
                .boxed()
                .compat()
                .from_err()
        }))
        .buffered(ENV_VARS.block_batch_size)
    }

    /// Request blocks by number through JSON-RPC.
    pub fn load_block_ptrs_by_numbers_rpc(
        &self,
        logger: Logger,
        numbers: Vec<BlockNumber>,
    ) -> impl futures03::Stream<Item = Result<Arc<ExtendedBlockPtr>, Error>> + Send {
        let web3 = self.web3.clone();
        let alloy = self.alloy.clone();

        futures03::stream::iter(numbers.into_iter().map(move |number| {
            let web3 = web3.clone();
            let alloy = alloy.clone();
            let logger = logger.clone();

            async move {
                retry(format!("load block {}", number), &logger)
                    .redact_log_urls(true)
                    .limit(ENV_VARS.request_retries)
                    .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                    .run(move || {
                        let web3 = web3.clone();
                        let alloy = alloy.clone();
                        let logger = logger.clone();

                        async move {
                            let block_result = web3
                                .eth()
                                .block(BlockId::Number(Web3BlockNumber::Number(number.into())))
                                .await;
                            let block_number =
                                alloy_rpc_types::BlockNumberOrTag::from(number as u64);
                            let block_result2 =
                                alloy.get_block_by_number(block_number).await.map(|block| {
                                    block.map(|bl| convert_block_hash_alloy2web3(&logger, bl))
                                });

                            let ret = match block_result {
                                Ok(Some(block)) => {
                                    assert_eq!(*block_result2.unwrap().unwrap(), block);
                                    let ptr = ExtendedBlockPtr::try_from((
                                        block.hash,
                                        block.number,
                                        block.parent_hash,
                                        block.timestamp,
                                    ))
                                    .map_err(|e| {
                                        anyhow::anyhow!("Failed to convert block: {}", e)
                                    })?;
                                    Ok(Arc::new(ptr))
                                }
                                Ok(None) => Err(anyhow::anyhow!(
                                    "Ethereum node did not find block with number {:?}",
                                    number
                                )),
                                Err(e) => Err(anyhow::anyhow!("Failed to fetch block: {}", e)),
                            };
                            ret
                        }
                    })
                    .await
                    .map_err(|e| match e {
                        TimeoutError::Elapsed => {
                            anyhow::anyhow!("Timeout while fetching block {}", number)
                        }
                        TimeoutError::Inner(e) => e,
                    })
            }
        }))
        .buffered(ENV_VARS.block_ptr_batch_size)
    }

    /// Request blocks ptrs for numbers through JSON-RPC.
    ///
    /// Reorg safety: If ids are numbers, they must be a final blocks.
    fn load_block_ptrs_rpc_alloy(
        &self,
        logger: Logger,
        block_nums: Vec<BlockNumber>,
    ) -> impl Stream<Item = BlockPtr, Error = Error> + Send {
        let alloy = self.alloy.clone();
        let logger = logger.clone();

        stream::iter_ok::<_, Error>(block_nums.into_iter().map(move |block_num| {
            let alloy = alloy.clone();
            retry(format!("load block ptr {}", block_num), &logger)
                .redact_log_urls(true)
                .when(|res| !res.is_ok() && !detect_null_block(res))
                .no_limit()
                .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                .run({
                    let logger = logger.clone();
                    let ret = move || {
                        let alloy = alloy.clone();
                        let logger = logger.clone();
                        async move {
                            let block =
                                Self::load_block_rpc_alloy(alloy, block_num as u64, &logger).await;
                            block.transpose().unwrap().map(|b| (*b).clone())
                        }
                    };
                    ret
                })
                .boxed()
                .compat()
                .from_err()
                .then(|res| {
                    if detect_null_block(&res) {
                        Ok(None)
                    } else {
                        Some(res).transpose()
                    }
                })
        }))
        .buffered(ENV_VARS.block_batch_size)
        .filter_map(|b| b)
        .map(|b| {
            let ret = b.into();
            ret
        })
    }

    /// Request blocks ptrs for numbers through JSON-RPC.
    ///
    /// Reorg safety: If ids are numbers, they must be a final blocks.
    fn load_block_ptrs_rpc(
        &self,
        logger: Logger,
        block_nums: Vec<BlockNumber>,
    ) -> impl Stream<Item = BlockPtr, Error = Error> + Send {
        let web3 = self.web3.clone();
        let logger = logger.clone();

        stream::iter_ok::<_, Error>(block_nums.into_iter().map(move |block_num| {
            let web3 = web3.clone();
            retry(format!("load block ptr {}", block_num), &logger)
                .redact_log_urls(true)
                .when(|res| !res.is_ok() && !detect_null_block(res))
                .no_limit()
                .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                .run({
                    let ret = move || {
                        let web3 = web3.clone();

                        async move {
                            let block = web3
                                .eth()
                                .block(BlockId::Number(Web3BlockNumber::Number(block_num.into())))
                                .boxed()
                                .await?;
                            block.ok_or_else(|| {
                                anyhow!("Ethereum node did not find block {:?}", block_num)
                            })
                        }
                    };
                    ret
                })
                .boxed()
                .compat()
                .from_err()
                .then(|res| {
                    if detect_null_block(&res) {
                        Ok(None)
                    } else {
                        Some(res).transpose()
                    }
                })
        }))
        .buffered(ENV_VARS.block_batch_size)
        .filter_map(|b| b)
        .map(|b| b.into())
    }

    /// Check if `block_ptr` refers to a block that is on the main chain, according to the Ethereum
    /// node.
    ///
    /// Careful: don't use this function without considering race conditions.
    /// Chain reorgs could happen at any time, and could affect the answer received.
    /// Generally, it is only safe to use this function with blocks that have received enough
    /// confirmations to guarantee no further reorgs, **and** where the Ethereum node is aware of
    /// those confirmations.
    /// If the Ethereum node is far behind in processing blocks, even old blocks can be subject to
    /// reorgs.
    pub(crate) async fn is_on_main_chain(
        &self,
        logger: &Logger,
        block_ptr: BlockPtr,
    ) -> Result<bool, Error> {
        // TODO: This considers null blocks, but we could instead bail if we encounter one as a
        // small optimization.
        let canonical_block = self
            .next_existing_ptr_to_number(logger, block_ptr.number)
            .await?;
        Ok(canonical_block == block_ptr)
    }

    pub(crate) fn logs_in_block_range(
        &self,
        logger: &Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        from: BlockNumber,
        to: BlockNumber,
        log_filter: EthereumLogFilter,
    ) -> DynTryFuture<'static, Vec<Log>, Error> {
        let eth: Self = self.cheap_clone();
        let logger = logger.clone();

        futures03::stream::iter(log_filter.eth_get_logs_filters().map(move |filter| {
            eth.cheap_clone().log_stream(
                logger.cheap_clone(),
                subgraph_metrics.cheap_clone(),
                from,
                to,
                filter,
            )
        }))
        // Real limits on the number of parallel requests are imposed within the adapter.
        .buffered(ENV_VARS.block_ingestor_max_concurrent_json_rpc_calls)
        .try_concat()
        .boxed()
    }

    pub(crate) fn calls_in_block_range<'a>(
        &self,
        logger: &Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        from: BlockNumber,
        to: BlockNumber,
        call_filter: &'a EthereumCallFilter,
    ) -> Box<dyn Stream<Item = EthereumCall, Error = Error> + Send + 'a> {
        info!(logger, "!!!! calls_in_block_range");
        let eth = self.clone();

        let EthereumCallFilter {
            contract_addresses_function_signatures,
            wildcard_signatures,
        } = call_filter;

        let mut addresses: Vec<H160> = contract_addresses_function_signatures
            .iter()
            .filter(|(_addr, (start_block, _fsigs))| start_block <= &to)
            .map(|(addr, (_start_block, _fsigs))| *addr)
            .collect::<HashSet<H160>>()
            .into_iter()
            .collect::<Vec<H160>>();

        if addresses.is_empty() && wildcard_signatures.is_empty() {
            // The filter has no started data sources in the requested range, nothing to do.
            // This prevents an expensive call to `trace_filter` with empty `addresses`.
            return Box::new(stream::empty());
        }

        // if wildcard_signatures is on, we can't filter by topic so we need to get all the traces.
        if addresses.len() > 100 || !wildcard_signatures.is_empty() {
            // If the address list is large, request all traces, this avoids generating huge
            // requests and potentially getting 413 errors.
            addresses = vec![];
        }

        Box::new(
            eth.trace_stream(logger, subgraph_metrics, from, to, addresses)
                .filter_map(|trace| EthereumCall::try_from_trace(&trace))
                .filter(move |call| {
                    // `trace_filter` can only filter by calls `to` an address and
                    // a block range. Since subgraphs are subscribing to calls
                    // for a specific contract function an additional filter needs
                    // to be applied
                    call_filter.matches(call)
                }),
        )
    }

    // Used to get the block triggers with a `polling` or `once` filter
    /// `polling_filter_type` is used to differentiate between `polling` and `once` filters
    /// A `polling_filter_type` value of  `BlockPollingFilterType::Once` is the case for
    /// intialization triggers
    /// A `polling_filter_type` value of  `BlockPollingFilterType::Polling` is the case for
    /// polling triggers
    pub(crate) fn blocks_matching_polling_intervals(
        &self,
        logger: Logger,
        from: i32,
        to: i32,
        filter: &EthereumBlockFilter,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<EthereumTrigger>, anyhow::Error>>
                + std::marker::Send,
        >,
    > {
        // Create a HashMap of block numbers to Vec<EthereumBlockTriggerType>
        let matching_blocks = (from..=to)
            .filter_map(|block_number| {
                filter
                    .polling_intervals
                    .iter()
                    .find_map(|(start_block, interval)| {
                        let has_once_trigger = (*interval == 0) && (block_number == *start_block);
                        let has_polling_trigger = block_number >= *start_block
                            && *interval > 0
                            && ((block_number - start_block) % *interval) == 0;

                        if has_once_trigger || has_polling_trigger {
                            let mut triggers = Vec::new();
                            if has_once_trigger {
                                triggers.push(EthereumBlockTriggerType::Start);
                            }
                            if has_polling_trigger {
                                triggers.push(EthereumBlockTriggerType::End);
                            }
                            Some((block_number, triggers))
                        } else {
                            None
                        }
                    })
            })
            .collect::<HashMap<_, _>>();

        let blocks_matching_polling_filter = self.load_ptrs_for_blocks(
            logger.clone(),
            matching_blocks.iter().map(|(k, _)| *k).collect_vec(),
        );

        let block_futures = blocks_matching_polling_filter.map(move |ptrs| {
            ptrs.into_iter()
                .flat_map(|ptr| {
                    let triggers = matching_blocks
                        .get(&ptr.number)
                        // Safe to unwrap since we are iterating over ptrs which was created from
                        // the keys of matching_blocks
                        .unwrap()
                        .iter()
                        .map(move |trigger| EthereumTrigger::Block(ptr.clone(), trigger.clone()));

                    triggers
                })
                .collect::<Vec<_>>()
        });

        block_futures.compat().boxed()
    }

    pub(crate) async fn calls_in_block(
        &self,
        logger: &Logger,
        subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
        block_number: BlockNumber,
        block_hash: H256,
    ) -> Result<Vec<EthereumCall>, Error> {
        let eth = self.clone();
        let addresses = Vec::new();
        let traces = eth
            .trace_stream(
                logger,
                subgraph_metrics.clone(),
                block_number,
                block_number,
                addresses,
            )
            .collect()
            .compat()
            .await?;

        // `trace_stream` returns all of the traces for the block, and this
        // includes a trace for the block reward which every block should have.
        // If there are no traces something has gone wrong.
        if traces.is_empty() {
            return Err(anyhow!(
                "Trace stream returned no traces for block: number = `{}`, hash = `{}`",
                block_number,
                block_hash,
            ));
        }

        // Since we can only pull traces by block number and we have
        // all the traces for the block, we need to ensure that the
        // block hash for the traces is equal to the desired block hash.
        // Assume all traces are for the same block.
        if traces.iter().nth(0).unwrap().block_hash != block_hash {
            return Err(anyhow!(
                "Trace stream returned traces for an unexpected block: \
                         number = `{}`, hash = `{}`",
                block_number,
                block_hash,
            ));
        }

        Ok(traces
            .iter()
            .filter_map(EthereumCall::try_from_trace)
            .collect())
    }

    /// Reorg safety: `to` must be a final block.
    pub(crate) fn block_range_to_ptrs(
        &self,
        logger: Logger,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Box<dyn Future<Item = Vec<BlockPtr>, Error = Error> + Send> {
        // Currently we can't go to the DB for this because there might be duplicate entries for
        // the same block number.
        debug!(&logger, "Requesting hashes for blocks [{}, {}]", from, to);
        Box::new(
            self.load_block_ptrs_rpc(logger, (from..=to).collect())
                .collect(),
        )
    }
    pub(crate) fn block_range_to_ptrs_alloy(
        &self,
        logger: Logger,
        from: BlockNumber,
        to: BlockNumber,
    ) -> Box<dyn Future<Item = Vec<BlockPtr>, Error = Error> + Send> {
        // Currently we can't go to the DB for this because there might be duplicate entries for
        // the same block number.
        debug!(&logger, "Requesting hashes for blocks [{}, {}]", from, to);
        Box::new(
            self.load_block_ptrs_rpc_alloy(logger, (from..=to).collect())
                .collect(),
        )
    }

    pub(crate) fn load_ptrs_for_blocks(
        &self,
        logger: Logger,
        blocks: Vec<BlockNumber>,
    ) -> Box<dyn Future<Item = Vec<BlockPtr>, Error = Error> + Send> {
        // Currently we can't go to the DB for this because there might be duplicate entries for
        // the same block number.
        debug!(&logger, "Requesting hashes for blocks {:?}", blocks);
        Box::new(self.load_block_ptrs_rpc(logger, blocks).collect())
    }

    pub async fn chain_id(&self) -> Result<u64, Error> {
        let logger = self.logger.clone();
        let web3 = self.web3.clone();
        let alloy = self.alloy.clone();
        u64::try_from(
            retry("chain_id RPC call", &logger)
                .redact_log_urls(true)
                .no_limit()
                .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                .run(move || {
                    let web3 = web3.cheap_clone();
                    let alloy = alloy.clone();
                    async move {
                        let ch = web3.eth().chain_id().await;
                        let ch2 = alloy.get_chain_id().await.map(u64_to_u256);
                        match (&ch, &ch2) {
                            (Ok(c1), Ok(c2)) => assert_eq!(c1, c2),
                            (_, _) => panic!("chain_id"),
                        };
                        ch
                    }
                })
                .await?,
        )
        .map_err(Error::msg)
    }
}

// Detects null blocks as can occur on Filecoin EVM chains, by checking for the FEVM-specific
// error returned when requesting such a null round. Ideally there should be a defined reponse or
// message for this case, or a check that is less dependent on the Filecoin implementation.
fn detect_null_block<T>(res: &Result<T, Error>) -> bool {
    match res {
        Ok(_) => false,
        Err(e) => e.to_string().contains("requested epoch was a null round"),
    }
}

impl EthereumAdapter {
    async fn latest_block_header_alloy(
        &self,
        logger: &Logger,
    ) -> Result<Arc<web3::types::Block<H256>>, IngestorError> {
        let alloy = self.alloy.clone();
        let logger2 = logger.clone();
        retry("eth_getBlockByNumber(latest) no txs RPC call", &logger2)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let alloy = alloy.clone();
                let logger = logger2.clone();
                async move {
                    let block_opt = Self::load_latest_block_rpc_alloy(alloy, &logger)
                        .await
                        .map_err(|e| anyhow!("could not get latest block from Ethereum: {}", e))?;

                    block_opt
                        .ok_or_else(|| anyhow!("no latest block returned from Ethereum").into())
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!("Ethereum node took too long to return latest block").into()
                })
            })
            .await
    }

    async fn latest_block_header_web3(
        &self,
        logger: &Logger,
    ) -> Result<web3::types::Block<H256>, IngestorError> {
        let web3 = self.web3.clone();
        retry("eth_getBlockByNumber(latest) no txs RPC call", logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                async move {
                    let block_opt = web3
                        .eth()
                        .block(Web3BlockNumber::Latest.into())
                        .await
                        .map_err(|e| anyhow!("could not get latest block from Ethereum: {}", e))?;

                    block_opt
                        .ok_or_else(|| anyhow!("no latest block returned from Ethereum").into())
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!("Ethereum node took too long to return latest block").into()
                })
            })
            .await
    }

    async fn block_by_hash_alloy(
        &self,
        logger: &Logger,
        block_hash: H256,
    ) -> Result<Option<LightEthereumBlock>, Error> {
        let alloy = self.alloy.clone();
        let logger = logger.clone();
        let retry_log_message = format!(
            "eth_getBlockByHash RPC call for block hash {:?}",
            block_hash
        );

        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let alloy = alloy.clone();
                let logger = logger.clone();
                async move {
                    Self::load_full_block_rpc_alloy(alloy.clone(), logger.clone(), block_hash)
                        .await
                        .map_err(Error::from)
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!("Ethereum node took too long to return block {}", block_hash)
                })
            })
            .await
            .map(|block| Some((*block).clone()))
    }

    async fn block_by_hash_web3(
        &self,
        logger: &Logger,
        block_hash: H256,
    ) -> Result<Option<LightEthereumBlock>, Error> {
        let web3 = self.web3.clone();
        let logger = logger.clone();
        let retry_log_message = format!(
            "eth_getBlockByHash RPC call for block hash {:?}",
            block_hash
        );

        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .limit(ENV_VARS.request_retries)
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                async move {
                    web3.eth()
                        .block_with_txs(BlockId::Hash(block_hash))
                        .await
                        .map_err(Error::from)
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!("Ethereum node took too long to return block {}", block_hash)
                })
            })
            .await
    }
}

#[async_trait]
impl EthereumAdapterTrait for EthereumAdapter {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn net_identifiers(&self) -> Result<ChainIdentifier, Error> {
        let logger = self.logger.clone();

        let web3 = self.web3.clone();
        let metrics = self.metrics.clone();
        let provider = self.provider().to_string();
        let net_version_future = retry("net_version RPC call", &logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(20)
            .run(move || {
                let web3 = web3.cheap_clone();
                let metrics = metrics.cheap_clone();
                let provider = provider.clone();
                async move {
                    web3.net().version().await.map_err(|e| {
                        metrics.set_status(ProviderStatus::VersionFail, &provider);
                        e.into()
                    })
                }
            })
            .map_err(|e| {
                self.metrics
                    .set_status(ProviderStatus::VersionTimeout, self.provider());
                e
            })
            .boxed();
        let alloy = self.alloy.clone();
        let metrics = self.metrics.clone();
        let provider = self.provider().to_string();
        let net_version_future2 = retry("net_version RPC call", &logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(20)
            .run(move || {
                let alloy = alloy.cheap_clone();
                let metrics = metrics.cheap_clone();
                let provider = provider.clone();
                async move {
                    alloy
                        .get_net_version()
                        .await
                        .map(|version| format!("{}", version))
                        .map_err(|e| {
                            metrics.set_status(ProviderStatus::VersionFail, &provider);
                            e.into()
                        })
                }
            })
            .map_err(|e| {
                self.metrics
                    .set_status(ProviderStatus::VersionTimeout, self.provider());
                e
            })
            .boxed();

        let web3 = self.web3.clone();
        let metrics = self.metrics.clone();
        let provider = self.provider().to_string();
        let retry_log_message = format!(
            "eth_getBlockByNumber({}, false) RPC call",
            ENV_VARS.genesis_block_number
        );
        let gen_block_hash_future = retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(30)
            .run(move || {
                let web3 = web3.cheap_clone();
                let metrics = metrics.cheap_clone();
                let provider = provider.clone();
                async move {
                    web3.eth()
                        .block(BlockId::Number(Web3BlockNumber::Number(
                            ENV_VARS.genesis_block_number.into(),
                        )))
                        .await
                        .map_err(|e| {
                            metrics.set_status(ProviderStatus::GenesisFail, &provider);
                            e
                        })?
                        .and_then(|gen_block| gen_block.hash.map(BlockHash::from))
                        .ok_or_else(|| anyhow!("Ethereum node could not find genesis block"))
                }
            })
            .map_err(|e| {
                self.metrics
                    .set_status(ProviderStatus::GenesisTimeout, self.provider());
                e
            });
        let alloy = self.alloy.clone();
        let logger2 = logger.clone();
        let metrics = self.metrics.clone();
        let provider = self.provider().to_string();
        let retry_log_message = format!(
            "eth_getBlockByNumber({}, false) RPC call",
            ENV_VARS.genesis_block_number
        );
        let gen_block_hash_future2 = retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(30)
            .run(move || {
                let alloy = alloy.cheap_clone();
                let logger = logger2.clone();
                let metrics = metrics.cheap_clone();
                let provider = provider.clone();
                async move {
                    Self::load_block_rpc_alloy(alloy, ENV_VARS.genesis_block_number, &logger)
                        .await
                        .map_err(|e| {
                            metrics.set_status(ProviderStatus::GenesisFail, &provider);
                            e
                        })?
                        .and_then(|gen_block| gen_block.hash.map(BlockHash::from))
                        .ok_or_else(|| anyhow!("Ethereum node could not find genesis block"))
                }
            })
            .map_err(|e| {
                self.metrics
                    .set_status(ProviderStatus::GenesisTimeout, self.provider());
                e
            });

        let (net_version, net_version2, genesis_block_hash, genesis_block_hash2) = try_join!(
            net_version_future,
            net_version_future2,
            gen_block_hash_future,
            gen_block_hash_future2
        )
        .map_err(|e| {
            anyhow!(
                "Ethereum node took too long to read network identifiers: {}",
                e
            )
        })?;

        let ident = ChainIdentifier {
            net_version,
            genesis_block_hash,
        };
        let ident2 = ChainIdentifier {
            net_version: net_version2,
            genesis_block_hash: genesis_block_hash2,
        };
        assert_eq!(ident, ident2);

        self.metrics
            .set_status(ProviderStatus::Working, self.provider());
        Ok(ident)
    }

    async fn latest_block_header(
        &self,
        logger: &Logger,
    ) -> Result<web3::types::Block<H256>, IngestorError> {
        let ret = self.latest_block_header_web3(logger).await;
        let ret2 = self.latest_block_header_alloy(logger).await;
        match (&ret, &ret2) {
            (Ok(bl1), Ok(bl2)) => {
                if bl1.number == bl2.number {
                    assert_eq!(Arc::new(bl1.clone()), *bl2)
                } else {
                    let diff =
                        bl1.number.unwrap().as_u32() as i64 - bl2.number.unwrap().as_u32() as i64;
                    assert!(diff > -30 && diff < 30)
                }
            }
            (a, b) => panic!("Not same types: {:?} and {:?}", a, b),
        };
        ret
    }

    async fn latest_block(&self, logger: &Logger) -> Result<LightEthereumBlock, IngestorError> {
        info!(logger, "!!!! latest_block");
        let web3 = self.web3.clone();
        retry("eth_getBlockByNumber(latest) with txs RPC call", logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                async move {
                    let block_opt = web3
                        .eth()
                        .block_with_txs(Web3BlockNumber::Latest.into())
                        .await
                        .map_err(|e| anyhow!("could not get latest block from Ethereum: {}", e))?;
                    block_opt
                        .ok_or_else(|| anyhow!("no latest block returned from Ethereum").into())
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!("Ethereum node took too long to return latest block").into()
                })
            })
            .await
    }

    async fn load_block(
        &self,
        logger: &Logger,
        block_hash: H256,
    ) -> Result<LightEthereumBlock, Error> {
        self.block_by_hash(logger, block_hash)
            .await?
            .ok_or_else(move || {
                anyhow!(
                    "Ethereum node could not find block with hash {}",
                    block_hash
                )
            })
    }

    async fn block_by_hash(
        &self,
        logger: &Logger,
        block_hash: H256,
    ) -> Result<Option<LightEthereumBlock>, Error> {
        let ret = self
            .block_by_hash_web3(logger, block_hash)
            .await?
            .ok_or_else(move || {
                anyhow!(
                    "Ethereum node could not find block with hash {}",
                    block_hash
                )
            });
        let ret2 = self
            .block_by_hash_alloy(logger, block_hash)
            .await?
            .ok_or_else(move || {
                anyhow!(
                    "Ethereum node could not find block with hash {}",
                    block_hash
                )
            });

        match (&ret, &ret2) {
            (Ok(r1), Ok(r2)) => {
                // assert_eq!(r1, r2);
                if !semi_equal(logger, r1, r2) {
                    // info!(logger, "RET1: {:?}", r1);
                    // info!(logger, "RET2: {:?}", r2);
                    panic!("Error - not equal!");
                }
            }
            (r1, r2) => {
                info!(logger, "RET1: {:?}", r1.is_ok());
                info!(logger, "RET2: {:?}", r2.is_ok());
                info!(logger, "Error - not same!");
            }
        }

        ret.map(Some)
    }

    async fn block_by_number(
        &self,
        logger: &Logger,
        block_number: BlockNumber,
    ) -> Result<Option<LightEthereumBlock>, Error> {
        info!(logger, "!!!! block_by_number");
        let web3 = self.web3.clone();
        let logger = logger.clone();
        let retry_log_message = format!(
            "eth_getBlockByNumber RPC call for block number {}",
            block_number
        );
        retry(retry_log_message, &logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                async move {
                    web3.eth()
                        .block_with_txs(BlockId::Number(block_number.into()))
                        .await
                        .map_err(Error::from)
                }
            })
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!(
                        "Ethereum node took too long to return block {}",
                        block_number
                    )
                })
            })
            .await
    }

    async fn load_full_block(
        &self,
        logger: &Logger,
        block: LightEthereumBlock,
    ) -> Result<EthereumBlock, IngestorError> {
        let web3 = Arc::clone(&self.web3);
        let alloy: Arc<dyn Provider + 'static> = self.alloy.clone();
        let logger = logger.clone();
        let block_hash = block.hash.expect("block is missing block hash");

        // The early return is necessary for correctness, otherwise we'll
        // request an empty batch which is not valid in JSON-RPC.
        if block.transactions.is_empty() {
            trace!(logger, "Block {} contains no transactions", block_hash);
            return Ok(EthereumBlock {
                block: Arc::new(block),
                transaction_receipts: Vec::new(),
            });
        }
        let hashes: Vec<_> = block.transactions.iter().map(|txn| txn.hash).collect();

        let supports_block_receipts = self
            .check_block_receipt_support_and_update_cache(
                alloy.clone(),
                web3.clone(),
                block_hash,
                self.supports_eip_1898,
                self.call_only,
                logger.clone(),
            )
            .await;

        // let log = logger.clone();
        let ret = fetch_receipts_with_retry(
            alloy,
            web3,
            hashes,
            block_hash,
            logger,
            supports_block_receipts,
        )
        .await
        .map(|transaction_receipts| EthereumBlock {
            block: Arc::new(block),
            transaction_receipts,
        });
        // info!(log, "load_full_block is OK: {}", ret.is_ok());
        ret
    }

    async fn block_hash_by_block_number(
        &self,
        logger: &Logger,
        block_number: BlockNumber,
    ) -> Result<Option<H256>, Error> {
        info!(logger, "!!!! block_hash_by_block_number");
        let web3 = self.web3.clone();
        let retry_log_message = format!(
            "eth_getBlockByNumber RPC call for block number {}",
            block_number
        );
        retry(retry_log_message, logger)
            .redact_log_urls(true)
            .no_limit()
            .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
            .run(move || {
                let web3 = web3.cheap_clone();
                async move {
                    web3.eth()
                        .block(BlockId::Number(block_number.into()))
                        .await
                        .map(|block_opt| block_opt.and_then(|block| block.hash))
                        .map_err(Error::from)
                }
            })
            .await
            .map_err(move |e| {
                e.into_inner().unwrap_or_else(move || {
                    anyhow!(
                        "Ethereum node took too long to return data for block #{}",
                        block_number
                    )
                })
            })
    }

    async fn get_balance(
        &self,
        logger: &Logger,
        address: H160,
        block_ptr: BlockPtr,
    ) -> Result<U256, EthereumRpcError> {
        debug!(
            logger, "eth_getBalance";
            "address" => format!("{}", address),
            "block" => format!("{}", block_ptr)
        );
        self.balance(logger, address, block_ptr).await
    }

    async fn get_code(
        &self,
        logger: &Logger,
        address: H160,
        block_ptr: BlockPtr,
    ) -> Result<Bytes, EthereumRpcError> {
        debug!(
            logger, "eth_getCode";
            "address" => format!("{}", address),
            "block" => format!("{}", block_ptr)
        );
        self.code(logger, address, block_ptr).await
    }

    async fn next_existing_ptr_to_number(
        &self,
        logger: &Logger,
        block_number: BlockNumber,
    ) -> Result<BlockPtr, Error> {
        let mut next_number = block_number;
        loop {
            let retry_log_message = format!(
                "eth_getBlockByNumber RPC call for block number {}",
                next_number
            );
            let web3 = self.web3.clone();
            let alloy = self.alloy.clone();
            let logger = logger.clone();
            let res = retry(retry_log_message, &logger)
                .redact_log_urls(true)
                .when(|res| !res.is_ok() && !detect_null_block(res))
                .no_limit()
                .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
                .run(move || {
                    let web3 = web3.cheap_clone();
                    let alloy = alloy.clone();
                    let logger = logger.clone();
                    async move {
                        let block1 = web3
                            .eth()
                            .block(BlockId::Number(next_number.into()))
                            .await
                            .map(|block_opt| block_opt.and_then(|block| block.hash))
                            .map_err(Error::from);
                        let block2 = Self::load_block_rpc_alloy(alloy, next_number as u64, &logger)
                            .await
                            .map(|block_opt| block_opt.and_then(|block| block.hash))
                            .map_err(Error::from);
                        match (&block1, &block2) {
                            (Ok(bl1), Ok(bl2)) => assert_eq!(bl1, bl2),
                            (_, _) => panic!("next_existing_ptr_to_number"),
                        };

                        block1
                    }
                })
                .await
                .map_err(move |e| {
                    e.into_inner().unwrap_or_else(move || {
                        anyhow!(
                            "Ethereum node took too long to return data for block #{}",
                            next_number
                        )
                    })
                });
            if detect_null_block(&res) {
                next_number += 1;
                continue;
            }
            return match res {
                Ok(Some(hash)) => Ok(BlockPtr::new(hash.into(), next_number)),
                Ok(None) => Err(anyhow!("Block {} does not contain hash", next_number)),
                Err(e) => Err(e),
            };
        }
    }

    async fn contract_call(
        &self,
        logger: &Logger,
        inp_call: &ContractCall,
        cache: Arc<dyn EthereumCallCache>,
    ) -> Result<(Option<Vec<abi::DynSolValue>>, call::Source), ContractCallError> {
        let mut result = self.contract_calls(logger, &[inp_call], cache).await?;
        // unwrap: self.contract_calls returns as many results as there were calls
        Ok(result.pop().unwrap())
    }

    async fn contract_calls(
        &self,
        logger: &Logger,
        calls: &[&ContractCall],
        cache: Arc<dyn EthereumCallCache>,
    ) -> Result<Vec<(Option<Vec<abi::DynSolValue>>, call::Source)>, ContractCallError> {
        fn as_req(
            logger: &Logger,
            call: &ContractCall,
            index: u32,
        ) -> Result<call::Request, ContractCallError> {
            // Emit custom error for type mismatches.
            for (val, kind) in call
                .args
                .iter()
                .zip(call.function.inputs.iter().map(|p| p.selector_type()))
            {
                let kind: abi::DynSolType = kind.parse().map_err(|err| {
                    ContractCallError::ABIError(anyhow!(
                        "failed to parse function input type '{kind}': {err}"
                    ))
                })?;

                if !val.type_check(&kind) {
                    return Err(ContractCallError::TypeError(val.clone(), kind.clone()));
                }
            }

            // Encode the call parameters according to the ABI
            let req = {
                let encoded_call = call
                    .function
                    .abi_encode_input(&call.args)
                    .map_err(|err| ContractCallError::EncodingError(err.into()))?;
                call::Request::new(call.address, encoded_call, index)
            };

            trace!(logger, "eth_call";
                "fn" => &call.function.name,
                "address" => hex::encode(call.address),
                "data" => hex::encode(req.encoded_call.as_ref()),
                "block_hash" => call.block_ptr.hash_hex(),
                "block_number" => call.block_ptr.block_number()
            );
            Ok(req)
        }

        fn decode(
            logger: &Logger,
            resp: call::Response,
            call: &ContractCall,
        ) -> (Option<Vec<abi::DynSolValue>>, call::Source) {
            let call::Response {
                retval,
                source,
                req: _,
            } = resp;
            use call::Retval::*;
            match retval {
                Value(output) => match call.function.abi_decode_output(&output) {
                    Ok(tokens) => (Some(tokens), source),
                    Err(e) => {
                        // Decode failures are reverts. The reasoning is that if Solidity fails to
                        // decode an argument, that's a revert, so the same goes for the output.
                        let reason = format!("failed to decode output: {}", e);
                        info!(logger, "Contract call reverted"; "reason" => reason);
                        (None, call::Source::Rpc)
                    }
                },
                Null => {
                    // We got a `0x` response. For old Geth, this can mean a revert. It can also be
                    // that the contract actually returned an empty response. A view call is meant
                    // to return something, so we treat empty responses the same as reverts.
                    info!(logger, "Contract call reverted"; "reason" => "empty response");
                    (None, call::Source::Rpc)
                }
            }
        }

        fn log_call_error(logger: &Logger, e: &ContractCallError, call: &ContractCall) {
            match e {
                ContractCallError::Web3Error(e) => error!(logger,
                    "Ethereum node returned an error when calling function \"{}\" of contract \"{}\": {}",
                    call.function.name, call.contract_name, e),
                ContractCallError::Timeout => error!(logger,
                    "Ethereum node did not respond when calling function \"{}\" of contract \"{}\"",
                    call.function.name, call.contract_name),
                _ => error!(logger,
                    "Failed to call function \"{}\" of contract \"{}\": {}",
                    call.function.name, call.contract_name, e),
            }
        }

        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let block_ptr = calls.first().unwrap().block_ptr.clone();
        if calls.iter().any(|call| call.block_ptr != block_ptr) {
            return Err(ContractCallError::Internal(
                "all calls must have the same block pointer".to_string(),
            ));
        }

        let reqs: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| as_req(logger, call, index as u32))
            .collect::<Result<_, _>>()?;

        let (mut resps, missing) = cache
            .get_calls(&reqs, block_ptr)
            .map_err(|e| error!(logger, "call cache get error"; "error" => e.to_string()))
            .unwrap_or_else(|_| (Vec::new(), reqs));

        let futs = missing.into_iter().map(|req| {
            let cache = cache.clone();
            async move {
                let call = calls[req.index as usize];
                match self.call_and_cache(logger, call, req, cache.clone()).await {
                    Ok(resp) => Ok(resp),
                    Err(e) => {
                        log_call_error(logger, &e, call);
                        Err(e)
                    }
                }
            }
        });
        resps.extend(try_join_all(futs).await?);

        // If we make it here, we have a response for every call.
        debug_assert_eq!(resps.len(), calls.len());

        // Bring the responses into the same order as the calls
        resps.sort_by_key(|resp| resp.req.index);

        let decoded: Vec<_> = resps
            .into_iter()
            .map(|res| {
                let call = &calls[res.req.index as usize];
                decode(logger, res, call)
            })
            .collect();

        Ok(decoded)
    }

    /// Load Ethereum blocks in bulk, returning results as they come back as a Stream.
    async fn load_blocks(
        &self,
        logger: Logger,
        chain_store: Arc<dyn ChainStore>,
        block_hashes: HashSet<H256>,
    ) -> Result<Vec<Arc<LightEthereumBlock>>, Error> {
        let block_hashes: Vec<_> = block_hashes.iter().cloned().collect();
        // Search for the block in the store first then use json-rpc as a backup.
        let mut blocks: Vec<Arc<LightEthereumBlock>> = chain_store
            .cheap_clone()
            .blocks(block_hashes.iter().map(|&b| b.into()).collect::<Vec<_>>())
            .await
            .map_err(|e| error!(&logger, "Error accessing block cache {}", e))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| json::from_value(value).ok())
            .map(Arc::new)
            .collect();

        let missing_blocks = Vec::from_iter(
            block_hashes
                .into_iter()
                .filter(|hash| !blocks.iter().any(|b| b.hash == Some(*hash))),
        );

        // Return a stream that lazily loads batches of blocks.
        info!(logger, "Requesting {} block(s)", missing_blocks.len());
        let new_blocks = self
            .load_blocks_rpc(logger.clone(), missing_blocks.clone())
            .collect()
            .compat()
            .await?;
        let new_blocks2 = self
            .load_blocks_rpc_alloy(logger.clone(), missing_blocks.clone())
            .collect()
            .compat()
            .await?;
        assert_eq!(new_blocks.len(), new_blocks2.len());
        for i in 0..new_blocks.len() {
            let mut bl1: web3::types::Block<Transaction> = (*new_blocks[i]).clone();
            fix_v_values(&mut bl1);
            let mut bl2: web3::types::Block<Transaction> = (*new_blocks[i]).clone();
            fix_v_values(&mut bl2);
            let str = format!("{:?}", bl1);
            let str2 = format!("{:?}", bl2);
            assert_eq!(str, str2);
        }
        let upsert_blocks: Vec<_> = new_blocks
            .iter()
            .map(|block| BlockFinality::Final(block.clone()))
            .collect();
        let block_refs: Vec<_> = upsert_blocks
            .iter()
            .map(|block| block as &dyn graph::blockchain::Block)
            .collect();
        if let Err(e) = chain_store.upsert_light_blocks(block_refs.as_slice()) {
            error!(logger, "Error writing to block cache {}", e);
        }
        blocks.extend(new_blocks);
        blocks.sort_by_key(|block| block.number);
        Ok(blocks)
    }
}

fn fix_v_values(bl1: &mut web3::types::Block<Transaction>) {
    for i in 0..bl1.transactions.len() {
        let v_new = if let Some(v) = bl1.transactions[i].v {
            Some(v % 62709)
        } else {
            None
        };
        bl1.transactions[i].v = v_new;
    }
}

/// Returns blocks with triggers, corresponding to the specified range and filters; and the resolved
/// `to` block, which is the nearest non-null block greater than or equal to the passed `to` block.
/// If a block contains no triggers, there may be no corresponding item in the stream.
/// However the (resolved) `to` block will always be present, even if triggers are empty.
///
/// Careful: don't use this function without considering race conditions.
/// Chain reorgs could happen at any time, and could affect the answer received.
/// Generally, it is only safe to use this function with blocks that have received enough
/// confirmations to guarantee no further reorgs, **and** where the Ethereum node is aware of
/// those confirmations.
/// If the Ethereum node is far behind in processing blocks, even old blocks can be subject to
/// reorgs.
/// It is recommended that `to` be far behind the block number of latest block the Ethereum
/// node is aware of.
pub(crate) async fn blocks_with_triggers(
    adapter: Arc<EthereumAdapter>,
    logger: Logger,
    chain_store: Arc<dyn ChainStore>,
    subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
    from: BlockNumber,
    to: BlockNumber,
    filter: &TriggerFilter,
    unified_api_version: UnifiedMappingApiVersion,
) -> Result<(Vec<BlockWithTriggers<crate::Chain>>, BlockNumber), Error> {
    // Each trigger filter needs to be queried for the same block range
    // and the blocks yielded need to be deduped. If any error occurs
    // while searching for a trigger type, the entire operation fails.
    let eth = adapter.clone();
    let call_filter = EthereumCallFilter::from(&filter.block);

    // Scan the block range to find relevant triggers
    let trigger_futs: FuturesUnordered<BoxFuture<Result<Vec<EthereumTrigger>, anyhow::Error>>> =
        FuturesUnordered::new();

    // Resolve the nearest non-null "to" block
    debug!(logger, "Finding nearest valid `to` block to {}", to);

    let to_ptr = eth.next_existing_ptr_to_number(&logger, to).await?;
    let to_hash = to_ptr.hash_as_h256();
    let to = to_ptr.block_number();

    // This is for `start` triggers which can be initialization handlers which needs to be run
    // before all other triggers
    if filter.block.trigger_every_block {
        let block_future = eth
            .block_range_to_ptrs(logger.clone(), from, to)
            .map(move |ptrs| {
                ptrs.into_iter()
                    .flat_map(|ptr| {
                        vec![
                            EthereumTrigger::Block(ptr.clone(), EthereumBlockTriggerType::Start),
                            EthereumTrigger::Block(ptr, EthereumBlockTriggerType::End),
                        ]
                    })
                    .collect()
            })
            .compat()
            .boxed();
        trigger_futs.push(block_future);

        //////////////////////////////////////////////////////////////////////////////////
        // Do comparison from here:
        //////////////////////////////////////////////////////////////////////////////////
        let trigger_futs1: FuturesUnordered<
            BoxFuture<Result<Vec<EthereumTrigger>, anyhow::Error>>,
        > = FuturesUnordered::new();
        let block_future1 = eth
            .block_range_to_ptrs(logger.clone(), from, to)
            .map(move |ptrs| {
                ptrs.into_iter()
                    .flat_map(|ptr| {
                        vec![
                            EthereumTrigger::Block(ptr.clone(), EthereumBlockTriggerType::Start),
                            EthereumTrigger::Block(ptr, EthereumBlockTriggerType::End),
                        ]
                    })
                    .collect()
            })
            .compat()
            .boxed();
        trigger_futs1.push(block_future1);
        let trigger_futs2: FuturesUnordered<
            BoxFuture<Result<Vec<EthereumTrigger>, anyhow::Error>>,
        > = FuturesUnordered::new();
        let block_future2 = eth
            .block_range_to_ptrs_alloy(logger.clone(), from, to)
            .map(move |ptrs| {
                ptrs.into_iter()
                    .flat_map(|ptr| {
                        vec![
                            EthereumTrigger::Block(ptr.clone(), EthereumBlockTriggerType::Start),
                            EthereumTrigger::Block(ptr, EthereumBlockTriggerType::End),
                        ]
                    })
                    .collect()
            })
            .compat()
            .boxed();
        trigger_futs2.push(block_future2);
        // request them
        let triggers1 = trigger_futs1
            .try_concat()
            .await
            .with_context(|| format!("Failed to obtain triggers for block {}", to))?;
        let block_hashes1: HashSet<H256> =
            triggers1.iter().map(EthereumTrigger::block_hash).collect();
        let triggers2 = trigger_futs2
            .try_concat()
            .await
            .with_context(|| format!("Failed to obtain triggers for block {}", to))?;
        let block_hashes2: HashSet<H256> =
            triggers2.iter().map(EthereumTrigger::block_hash).collect();
        assert_eq!(block_hashes1, block_hashes2)
        //////////////////////////////////////////////////////////////////////////////////
        // to here
        //////////////////////////////////////////////////////////////////////////////////
    } else if !filter.block.polling_intervals.is_empty() {
        let block_futures_matching_once_filter =
            eth.blocks_matching_polling_intervals(logger.clone(), from, to, &filter.block);
        trigger_futs.push(block_futures_matching_once_filter);
    }

    // Scan for Logs
    if !filter.log.is_empty() {
        let logs_future = get_logs_and_transactions(
            &eth,
            &logger,
            subgraph_metrics.clone(),
            from,
            to,
            filter.log.clone(),
            &unified_api_version,
        )
        .boxed();
        trigger_futs.push(logs_future)
    }
    // Scan for Calls
    if !filter.call.is_empty() {
        let calls_future = eth
            .calls_in_block_range(&logger, subgraph_metrics.clone(), from, to, &filter.call)
            .map(Arc::new)
            .map(EthereumTrigger::Call)
            .collect()
            .compat()
            .boxed();
        trigger_futs.push(calls_future)
    }

    if !filter.block.contract_addresses.is_empty() {
        // To determine which blocks include a call to addresses
        // in the block filter, transform the `block_filter` into
        // a `call_filter` and run `blocks_with_calls`
        let block_future = eth
            .calls_in_block_range(&logger, subgraph_metrics.clone(), from, to, &call_filter)
            .map(|call| {
                EthereumTrigger::Block(
                    BlockPtr::from(&call),
                    EthereumBlockTriggerType::WithCallTo(call.to),
                )
            })
            .collect()
            .compat()
            .boxed();
        trigger_futs.push(block_future)
    }

    // Join on triggers, unpack and handle possible errors
    let triggers = trigger_futs
        .try_concat()
        .await
        .with_context(|| format!("Failed to obtain triggers for block {}", to))?;

    // info!(logger, "TRIGGERS:");
    // triggers.iter().for_each(|t| info!(logger, "TR: {:?}", t));

    let mut block_hashes: HashSet<H256> =
        triggers.iter().map(EthereumTrigger::block_hash).collect();
    let mut triggers_by_block: HashMap<BlockNumber, Vec<EthereumTrigger>> =
        triggers.into_iter().fold(HashMap::new(), |mut map, t| {
            map.entry(t.block_number()).or_default().push(t);
            map
        });

    debug!(logger, "Found {} relevant block(s)", block_hashes.len());

    // Make sure `to` is included, even if empty.
    block_hashes.insert(to_hash);
    triggers_by_block.entry(to).or_default();

    let logger2 = logger.cheap_clone();

    let blocks: Vec<_> = eth
        .load_blocks(logger.cheap_clone(), chain_store.clone(), block_hashes)
        .await?
        .into_iter()
        .map(
            move |block| match triggers_by_block.remove(&(block.number() as BlockNumber)) {
                Some(triggers) => Ok(BlockWithTriggers::new(
                    BlockFinality::Final(block),
                    triggers,
                    &logger2,
                )),
                None => Err(anyhow!(
                    "block {} not found in `triggers_by_block`",
                    block.block_ptr()
                )),
            },
        )
        .collect::<Result<_, _>>()?;

    // Filter out call triggers that come from unsuccessful transactions
    let futures = blocks.into_iter().map(|block| {
        filter_call_triggers_from_unsuccessful_transactions(block, &eth, &chain_store, &logger)
    });
    let mut blocks = futures03::future::try_join_all(futures).await?;

    blocks.sort_by_key(|block| block.ptr().number);

    // Sanity check that the returned blocks are in the correct range.
    // Unwrap: `blocks` always includes at least `to`.
    let first = blocks.first().unwrap().ptr().number;
    let last = blocks.last().unwrap().ptr().number;
    if first < from {
        return Err(anyhow!(
            "block {} returned by the Ethereum node is before {}, the first block of the requested range",
            first,
            from,
        ));
    }
    if last > to {
        return Err(anyhow!(
            "block {} returned by the Ethereum node is after {}, the last block of the requested range",
            last,
            to,
        ));
    }

    Ok((blocks, to))
}

pub(crate) async fn get_calls(
    client: &Arc<ChainClient<Chain>>,
    logger: Logger,
    subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
    capabilities: &NodeCapabilities,
    requires_traces: bool,
    block: BlockFinality,
) -> Result<BlockFinality, Error> {
    // For final blocks, or nonfinal blocks where we already checked
    // (`calls.is_some()`), do nothing; if we haven't checked for calls, do
    // that now
    match block {
        BlockFinality::Final(_)
        | BlockFinality::NonFinal(EthereumBlockWithCalls {
            ethereum_block: _,
            calls: Some(_),
        }) => Ok(block),
        BlockFinality::NonFinal(EthereumBlockWithCalls {
            ethereum_block,
            calls: None,
        }) => {
            let calls = if !requires_traces || ethereum_block.transaction_receipts.is_empty() {
                vec![]
            } else {
                client
                    .rpc()?
                    .cheapest_with(capabilities)
                    .await?
                    .calls_in_block(
                        &logger,
                        subgraph_metrics.clone(),
                        BlockNumber::try_from(ethereum_block.block.number.unwrap().as_u64())
                            .unwrap(),
                        ethereum_block.block.hash.unwrap(),
                    )
                    .await?
            };
            Ok(BlockFinality::NonFinal(EthereumBlockWithCalls {
                ethereum_block,
                calls: Some(calls),
            }))
        }
        BlockFinality::Ptr(_) => {
            unreachable!("get_calls called with BlockFinality::Ptr")
        }
    }
}

pub(crate) fn parse_log_triggers(
    log_filter: &EthereumLogFilter,
    block: &EthereumBlock,
) -> Vec<EthereumTrigger> {
    if log_filter.is_empty() {
        return vec![];
    }

    block
        .transaction_receipts
        .iter()
        .flat_map(move |receipt| {
            receipt.logs.iter().enumerate().map(move |(index, _)| {
                EthereumTrigger::Log(LogRef::LogPosition(index, receipt.cheap_clone()))
            })
        })
        .collect()
}

pub(crate) fn parse_call_triggers(
    call_filter: &EthereumCallFilter,
    block: &EthereumBlockWithCalls,
) -> anyhow::Result<Vec<EthereumTrigger>> {
    if call_filter.is_empty() {
        return Ok(vec![]);
    }

    match &block.calls {
        Some(calls) => calls
            .iter()
            .filter(move |call| call_filter.matches(call))
            .map(
                move |call| match block.transaction_for_call_succeeded(call) {
                    Ok(true) => Ok(Some(EthereumTrigger::Call(Arc::new(call.clone())))),
                    Ok(false) => Ok(None),
                    Err(e) => Err(e),
                },
            )
            .filter_map_ok(|some_trigger| some_trigger)
            .collect(),
        None => Ok(vec![]),
    }
}

/// This method does not parse block triggers with `once` filters.
/// This is because it is to be run before any other triggers are run.
/// So we have `parse_initialization_triggers` for that.
pub(crate) fn parse_block_triggers(
    block_filter: &EthereumBlockFilter,
    block: &EthereumBlockWithCalls,
) -> Vec<EthereumTrigger> {
    if block_filter.is_empty() {
        return vec![];
    }

    let block_ptr = BlockPtr::from(&block.ethereum_block);
    let trigger_every_block = block_filter.trigger_every_block;
    let call_filter = EthereumCallFilter::from(block_filter);
    let block_ptr2 = block_ptr.cheap_clone();
    let block_ptr3 = block_ptr.cheap_clone();
    let block_number = block_ptr.number;

    let mut triggers = match &block.calls {
        Some(calls) => calls
            .iter()
            .filter(move |call| call_filter.matches(call))
            .map(move |call| {
                EthereumTrigger::Block(
                    block_ptr2.clone(),
                    EthereumBlockTriggerType::WithCallTo(call.to),
                )
            })
            .collect::<Vec<EthereumTrigger>>(),
        None => vec![],
    };
    if trigger_every_block {
        triggers.push(EthereumTrigger::Block(
            block_ptr.clone(),
            EthereumBlockTriggerType::Start,
        ));
        triggers.push(EthereumTrigger::Block(
            block_ptr,
            EthereumBlockTriggerType::End,
        ));
    } else if !block_filter.polling_intervals.is_empty() {
        let has_polling_trigger =
            &block_filter
                .polling_intervals
                .iter()
                .any(|(start_block, interval)| match interval {
                    0 => false,
                    _ => {
                        block_number >= *start_block
                            && (block_number - *start_block) % *interval == 0
                    }
                });

        let has_once_trigger =
            &block_filter
                .polling_intervals
                .iter()
                .any(|(start_block, interval)| match interval {
                    0 => block_number == *start_block,
                    _ => false,
                });

        if *has_once_trigger {
            triggers.push(EthereumTrigger::Block(
                block_ptr3.clone(),
                EthereumBlockTriggerType::Start,
            ));
        }

        if *has_polling_trigger {
            triggers.push(EthereumTrigger::Block(
                block_ptr3,
                EthereumBlockTriggerType::End,
            ));
        }
    }
    triggers
}

async fn fetch_receipt_from_ethereum_client(
    eth: &EthereumAdapter,
    transaction_hash: &H256,
) -> anyhow::Result<TransactionReceipt> {
    println!("!!!! fetch_receipt_from_ethereum_client");
    match eth.web3.eth().transaction_receipt(*transaction_hash).await {
        Ok(Some(receipt)) => Ok(receipt),
        Ok(None) => bail!("Could not find transaction receipt"),
        Err(error) => bail!("Failed to fetch transaction receipt: {}", error),
    }
}

async fn filter_call_triggers_from_unsuccessful_transactions(
    mut block: BlockWithTriggers<crate::Chain>,
    eth: &EthereumAdapter,
    chain_store: &Arc<dyn ChainStore>,
    logger: &Logger,
) -> anyhow::Result<BlockWithTriggers<crate::Chain>> {
    // Return early if there is no trigger data
    if block.trigger_data.is_empty() {
        return Ok(block);
    }

    let initial_number_of_triggers = block.trigger_data.len();

    // Get the transaction hash from each call trigger
    let transaction_hashes: BTreeSet<H256> = block
        .trigger_data
        .iter()
        .filter_map(|trigger| match trigger.as_chain() {
            Some(EthereumTrigger::Call(call_trigger)) => Some(call_trigger.transaction_hash),
            _ => None,
        })
        .collect::<Option<BTreeSet<H256>>>()
        .ok_or(anyhow!(
            "failed to obtain transaction hash from call triggers"
        ))?;

    // Return early if there are no transaction hashes
    if transaction_hashes.is_empty() {
        return Ok(block);
    }

    // And obtain all Transaction values for the calls in this block.
    let transactions: Vec<&Transaction> = {
        match &block.block {
            BlockFinality::Final(ref block) => block
                .transactions
                .iter()
                .filter(|transaction| transaction_hashes.contains(&transaction.hash))
                .collect(),
            BlockFinality::NonFinal(_block_with_calls) => {
                unreachable!(
                    "this function should not be called when dealing with non-final blocks"
                )
            }
            BlockFinality::Ptr(_block) => {
                unreachable!(
                    "this function should not be called when dealing with header-only blocks"
                )
            }
        }
    };

    // Confidence check: Did we collect all transactions for the current call triggers?
    if transactions.len() != transaction_hashes.len() {
        bail!("failed to find transactions in block for the given call triggers")
    }

    // We'll also need the receipts for those transactions. In this step we collect all receipts
    // we have in store for the current block.
    let mut receipts = chain_store
        .transaction_receipts_in_block(&block.ptr().hash_as_h256())
        .await?
        .into_iter()
        .map(|receipt| (receipt.transaction_hash, receipt))
        .collect::<BTreeMap<H256, LightTransactionReceipt>>();

    // Do we have a receipt for each transaction under analysis?
    let mut receipts_and_transactions: Vec<(&Transaction, LightTransactionReceipt)> = Vec::new();
    let mut transactions_without_receipt: Vec<&Transaction> = Vec::new();
    for transaction in transactions.iter() {
        if let Some(receipt) = receipts.remove(&transaction.hash) {
            receipts_and_transactions.push((transaction, receipt));
        } else {
            transactions_without_receipt.push(transaction);
        }
    }

    // When some receipts are missing, we then try to fetch them from our client.
    let futures = transactions_without_receipt
        .iter()
        .map(|transaction| async move {
            fetch_receipt_from_ethereum_client(eth, &transaction.hash)
                .await
                .map(|receipt| (transaction, receipt))
        });
    futures03::future::try_join_all(futures)
        .await?
        .into_iter()
        .for_each(|(transaction, receipt)| {
            receipts_and_transactions.push((transaction, receipt.into()))
        });

    // TODO: We should persist those fresh transaction receipts into the store, so we don't incur
    // additional Ethereum API calls for future scans on this block.

    // With all transactions and receipts in hand, we can evaluate the success of each transaction
    let mut transaction_success: BTreeMap<&H256, bool> = BTreeMap::new();
    for (transaction, receipt) in receipts_and_transactions.into_iter() {
        transaction_success.insert(
            &transaction.hash,
            evaluate_transaction_status(receipt.status),
        );
    }

    // Confidence check: Did we inspect the status of all transactions?
    if !transaction_hashes
        .iter()
        .all(|tx| transaction_success.contains_key(tx))
    {
        bail!("Not all transactions status were inspected")
    }

    // Filter call triggers from unsuccessful transactions
    block.trigger_data.retain(|trigger| {
        if let Some(EthereumTrigger::Call(call_trigger)) = trigger.as_chain() {
            // Unwrap: We already checked that those values exist
            transaction_success[&call_trigger.transaction_hash.unwrap()]
        } else {
            // We are not filtering other types of triggers
            true
        }
    });

    // Log if any call trigger was filtered out
    let final_number_of_triggers = block.trigger_data.len();
    let number_of_filtered_triggers = initial_number_of_triggers - final_number_of_triggers;
    if number_of_filtered_triggers != 0 {
        let noun = {
            if number_of_filtered_triggers == 1 {
                "call trigger"
            } else {
                "call triggers"
            }
        };
        info!(&logger,
              "Filtered {} {} from failed transactions", number_of_filtered_triggers, noun ;
              "block_number" => block.ptr().block_number());
    }
    Ok(block)
}

/// Deprecated. Wraps the [`fetch_transaction_receipts_in_batch`] in a retry loop.
async fn fetch_transaction_receipts_in_batch_with_retry(
    web3: Arc<Web3<Transport>>,
    hashes: Vec<H256>,
    block_hash: H256,
    logger: Logger,
) -> Result<Vec<Arc<TransactionReceipt>>, IngestorError> {
    info!(
        logger,
        "!!!! fetch_transaction_receipts_in_batch_with_retry"
    );
    let retry_log_message = format!(
        "batch eth_getTransactionReceipt RPC call for block {:?}",
        block_hash
    );
    retry(retry_log_message, &logger)
        .redact_log_urls(true)
        .limit(ENV_VARS.request_retries)
        .no_logging()
        .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
        .run(move || {
            let web3 = web3.cheap_clone();
            let hashes = hashes.clone();
            let logger = logger.cheap_clone();
            fetch_transaction_receipts_in_batch(web3, hashes, block_hash, logger).boxed()
        })
        .await
        .map_err(|_timeout| anyhow!(block_hash).into())
}

/// Deprecated. Attempts to fetch multiple transaction receipts in a batching contex.
async fn fetch_transaction_receipts_in_batch(
    web3: Arc<Web3<Transport>>,
    hashes: Vec<H256>,
    block_hash: H256,
    logger: Logger,
) -> Result<Vec<Arc<TransactionReceipt>>, IngestorError> {
    info!(logger, "!!!! fetch_transaction_receipts_in_batch");
    let batching_web3 = Web3::new(Batch::new(web3.transport().clone()));
    let eth = batching_web3.eth();
    let receipt_futures = hashes
        .into_iter()
        .map(move |hash| {
            let logger = logger.cheap_clone();
            eth.transaction_receipt(hash)
                .map_err(IngestorError::from)
                .and_then(move |some_receipt| async move {
                    resolve_transaction_receipt(some_receipt, hash, block_hash, logger)
                })
        })
        .collect::<Vec<_>>();

    batching_web3.transport().submit_batch().await?;

    let mut collected = vec![];
    for receipt in receipt_futures.into_iter() {
        collected.push(Arc::new(receipt.await?))
    }
    Ok(collected)
}

pub(crate) async fn check_block_receipt_support(
    alloy: Arc<dyn Provider + 'static>,
    web3: Arc<Web3<impl web3::Transport>>,
    block_hash: H256,
    supports_eip_1898: bool,
    call_only: bool,
) -> Result<(), Error> {
    if call_only {
        return Err(anyhow!("Provider is call-only"));
    }

    if !supports_eip_1898 {
        return Err(anyhow!("Provider does not support EIP 1898"));
    }

    // Fetch block receipts from the provider for the latest block.
    let block_receipts_result = web3.eth().block_receipts(BlockId::Hash(block_hash)).await;
    let hash: alloy_rpc_types::BlockId =
        alloy_rpc_types::BlockId::hash(B256::new(*block_hash.as_fixed_bytes()));
    let block_receipts_result2 = alloy.get_block_receipts(hash).await;

    // Determine if the provider supports block receipts based on the fetched result.
    let ret = match block_receipts_result {
        Ok(Some(receipts)) if !receipts.is_empty() => Ok(()),
        Ok(_) => Err(anyhow!("Block receipts are empty")),
        Err(err) => Err(anyhow!("Error fetching block receipts: {}", err)),
    };
    let ret2 = match block_receipts_result2 {
        Ok(Some(receipts)) if !receipts.is_empty() => Ok(()),
        Ok(_) => Err(anyhow!("Block receipts are empty")),
        Err(err) => Err(anyhow!("Error fetching block receipts: {}", err)),
    };
    assert_eq!(ret.is_ok(), ret2.is_ok());
    ret
}

// Fetches transaction receipts with retries. This function acts as a dispatcher
// based on whether block receipts are supported or individual transaction receipts
// need to be fetched.
async fn fetch_receipts_with_retry(
    alloy: Arc<dyn Provider + 'static>,
    web3: Arc<Web3<Transport>>,
    hashes: Vec<H256>,
    block_hash: H256,
    logger: Logger,
    supports_block_receipts: bool,
) -> Result<Vec<Arc<TransactionReceipt>>, IngestorError> {
    if supports_block_receipts {
        return fetch_block_receipts_with_retry(alloy, web3, hashes, block_hash, logger).await;
    }
    fetch_individual_receipts_with_retry(web3, hashes, block_hash, logger).await
}

// Fetches receipts for each transaction in the block individually.
async fn fetch_individual_receipts_with_retry(
    web3: Arc<Web3<Transport>>,
    hashes: Vec<H256>,
    block_hash: H256,
    logger: Logger,
) -> Result<Vec<Arc<TransactionReceipt>>, IngestorError> {
    if ENV_VARS.fetch_receipts_in_batches {
        return fetch_transaction_receipts_in_batch_with_retry(web3, hashes, block_hash, logger)
            .await;
    }

    // Use a stream to fetch receipts individually
    let hash_stream = graph::tokio_stream::iter(hashes);
    let receipt_stream = hash_stream
        .map(move |tx_hash| {
            fetch_transaction_receipt_with_retry(
                web3.cheap_clone(),
                tx_hash,
                block_hash,
                logger.cheap_clone(),
            )
        })
        .buffered(ENV_VARS.block_ingestor_max_concurrent_json_rpc_calls);

    graph::tokio_stream::StreamExt::collect::<Result<Vec<Arc<TransactionReceipt>>, IngestorError>>(
        receipt_stream,
    )
    .await
}

/// Fetches transaction receipts of all transactions in a block with `eth_getBlockReceipts` call.
async fn fetch_block_receipts_with_retry(
    alloy: Arc<dyn Provider + 'static>,
    web3: Arc<Web3<Transport>>,
    hashes: Vec<H256>,
    block_hash: H256,
    logger: Logger,
) -> Result<Vec<Arc<TransactionReceipt>>, IngestorError> {
    let logger = logger.cheap_clone();
    let retry_log_message = format!("eth_getBlockReceipts RPC call for block {:?}", block_hash);

    // Perform the retry operation
    let receipts_option = retry(retry_log_message.clone(), &logger)
        .redact_log_urls(true)
        .limit(ENV_VARS.request_retries)
        .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
        .run({
            let block_hash = block_hash.clone();
            move || web3.eth().block_receipts(BlockId::Hash(block_hash)).boxed()
        })
        .await
        .map_err(|_timeout| -> IngestorError { anyhow!(block_hash).into() })?;
    // Perform the retry operation
    let receipts_option2 = retry(retry_log_message, &logger)
        .redact_log_urls(true)
        .limit(ENV_VARS.request_retries)
        .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
        .run(move || {
            let hash: alloy_rpc_types::BlockId =
                alloy_rpc_types::BlockId::hash(B256::new(*block_hash.as_fixed_bytes()));
            alloy.get_block_receipts(hash)
        })
        .await
        .map_err(|_timeout| -> IngestorError { anyhow!(block_hash).into() })?;
    let receipts_option3: Option<Vec<TransactionReceipt>> = convert_receipts(receipts_option2);
    match (&receipts_option, &receipts_option3) {
        (Some(r1), Some(r2)) => {
            assert_eq!(r1.len(), r2.len());
            for i in 0..r1.len() {
                let mut rec1 = r1[i].clone();
                rec1.cumulative_gas_used = u64_to_u256(0);
                rec1.transaction_type = None;
                rec1.root = None;
                let mut rec2 = r2[i].clone();
                rec2.cumulative_gas_used = u64_to_u256(0);
                rec2.transaction_type = None;
                rec2.root = None;
                assert_eq!(rec1, rec2)
            }
        }
        (_, _) => info!(logger, "One side of receipes are missing"),
    };

    // Check if receipts are available, and transform them if they are
    match receipts_option {
        Some(receipts) => {
            // Create a HashSet from the transaction hashes of the receipts
            let receipt_hashes_set: HashSet<_> =
                receipts.iter().map(|r| r.transaction_hash).collect();

            // Check if the set contains all the hashes and has the same length as the hashes vec
            if hashes.len() == receipt_hashes_set.len()
                && hashes.iter().all(|hash| receipt_hashes_set.contains(hash))
            {
                let transformed_receipts = receipts.into_iter().map(Arc::new).collect();
                Ok(transformed_receipts)
            } else {
                // If there's a mismatch in numbers or a missing hash, return an error
                Err(IngestorError::BlockReceiptsMismatched(block_hash))
            }
        }
        None => {
            // If no receipts are found, return an error
            Err(IngestorError::BlockReceiptsUnavailable(block_hash))
        }
    }
}

/// Retries fetching a single transaction receipt.
async fn fetch_transaction_receipt_with_retry(
    web3: Arc<Web3<Transport>>,
    transaction_hash: H256,
    block_hash: H256,
    logger: Logger,
) -> Result<Arc<TransactionReceipt>, IngestorError> {
    info!(logger, "!!!! fetch_transaction_receipt_with_retry");
    let logger = logger.cheap_clone();
    let retry_log_message = format!(
        "eth_getTransactionReceipt RPC call for transaction {:?}",
        transaction_hash
    );
    retry(retry_log_message, &logger)
        .redact_log_urls(true)
        .limit(ENV_VARS.request_retries)
        .timeout_secs(ENV_VARS.json_rpc_timeout.as_secs())
        .run(move || web3.eth().transaction_receipt(transaction_hash).boxed())
        .await
        .map_err(|_timeout| anyhow!(block_hash).into())
        .and_then(move |some_receipt| {
            resolve_transaction_receipt(some_receipt, transaction_hash, block_hash, logger)
        })
        .map(Arc::new)
}

fn resolve_transaction_receipt(
    transaction_receipt: Option<TransactionReceipt>,
    transaction_hash: H256,
    block_hash: H256,
    logger: Logger,
) -> Result<TransactionReceipt, IngestorError> {
    match transaction_receipt {
        // A receipt might be missing because the block was uncled, and the transaction never
        // made it back into the main chain.
        Some(receipt) => {
            // Check if the receipt has a block hash and is for the right block. Parity nodes seem
            // to return receipts with no block hash when a transaction is no longer in the main
            // chain, so treat that case the same as a receipt being absent entirely.
            //
            // Also as a sanity check against provider nonsense, check that the receipt transaction
            // hash and the requested transaction hash match.
            if receipt.block_hash != Some(block_hash)
                || transaction_hash != receipt.transaction_hash
            {
                info!(
                    logger, "receipt block mismatch";
                    "receipt_block_hash" =>
                    receipt.block_hash.unwrap_or_default().to_string(),
                    "block_hash" =>
                        block_hash.to_string(),
                    "tx_hash" => transaction_hash.to_string(),
                    "receipt_tx_hash" => receipt.transaction_hash.to_string(),
                );

                // If the receipt came from a different block, then the Ethereum node no longer
                // considers this block to be in the main chain. Nothing we can do from here except
                // give up trying to ingest this block. There is no way to get the transaction
                // receipt from this block.
                Err(IngestorError::BlockUnavailable(block_hash))
            } else {
                Ok(receipt)
            }
        }
        None => {
            // No receipt was returned.
            //
            // This can be because the Ethereum node no longer considers this block to be part of
            // the main chain, and so the transaction is no longer in the main chain. Nothing we can
            // do from here except give up trying to ingest this block.
            //
            // This could also be because the receipt is simply not available yet. For that case, we
            // should retry until it becomes available.
            Err(IngestorError::ReceiptUnavailable(
                block_hash,
                transaction_hash,
            ))
        }
    }
}

/// Retrieves logs and the associated transaction receipts, if required by the [`EthereumLogFilter`].
async fn get_logs_and_transactions(
    adapter: &Arc<EthereumAdapter>,
    logger: &Logger,
    subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
    from: BlockNumber,
    to: BlockNumber,
    log_filter: EthereumLogFilter,
    unified_api_version: &UnifiedMappingApiVersion,
) -> Result<Vec<EthereumTrigger>, anyhow::Error> {
    // Obtain logs externally
    let logs = adapter
        .logs_in_block_range(
            logger,
            subgraph_metrics.cheap_clone(),
            from,
            to,
            log_filter.clone(),
        )
        .await?;

    // Not all logs have associated transaction hashes, nor do all triggers require them.
    // We also restrict receipts retrieval for some api versions.
    let transaction_hashes_by_block: HashMap<H256, HashSet<H256>> = logs
        .iter()
        .filter(|_| unified_api_version.equal_or_greater_than(&API_VERSION_0_0_7))
        .filter(|log| {
            if let Some(signature) = log.topics.first() {
                log_filter.requires_transaction_receipt(signature, Some(&log.address), &log.topics)
            } else {
                false
            }
        })
        .filter_map(|log| {
            if let (Some(block), Some(txn)) = (log.block_hash, log.transaction_hash) {
                Some((block, txn))
            } else {
                // Absent block and transaction data might happen for pending transactions, which we
                // don't handle.
                None
            }
        })
        .fold(
            HashMap::<H256, HashSet<H256>>::new(),
            |mut acc, (block_hash, txn_hash)| {
                acc.entry(block_hash).or_default().insert(txn_hash);
                acc
            },
        );

    // Obtain receipts externally
    let transaction_receipts_by_hash = get_transaction_receipts_for_transaction_hashes(
        adapter,
        &transaction_hashes_by_block,
        subgraph_metrics,
        logger.cheap_clone(),
    )
    .await?;

    // Associate each log with its receipt, when possible
    let mut log_triggers = Vec::new();
    for log in logs.into_iter() {
        let optional_receipt = log
            .transaction_hash
            .and_then(|txn| transaction_receipts_by_hash.get(&txn).cloned());
        let value = EthereumTrigger::Log(LogRef::FullLog(Arc::new(log), optional_receipt));
        log_triggers.push(value);
    }

    Ok(log_triggers)
}

/// Tries to retrive all transaction receipts for a set of transaction hashes.
async fn get_transaction_receipts_for_transaction_hashes(
    adapter: &EthereumAdapter,
    transaction_hashes_by_block: &HashMap<H256, HashSet<H256>>,
    subgraph_metrics: Arc<SubgraphEthRpcMetrics>,
    logger: Logger,
) -> Result<HashMap<H256, Arc<TransactionReceipt>>, anyhow::Error> {
    use std::collections::hash_map::Entry::Vacant;

    let mut receipts_by_hash: HashMap<H256, Arc<TransactionReceipt>> = HashMap::new();

    // Return early if input set is empty
    if transaction_hashes_by_block.is_empty() {
        return Ok(receipts_by_hash);
    }
    info!(
        logger,
        "!!!! get_transaction_receipts_for_transaction_hashes"
    );
    // Keep a record of all unique transaction hashes for which we'll request receipts. We will
    // later use this to check if we have collected the receipts from all required transactions.
    let mut unique_transaction_hashes: HashSet<&H256> = HashSet::new();

    // Request transaction receipts concurrently
    let receipt_futures = FuturesUnordered::new();

    let web3 = Arc::clone(&adapter.web3);
    for (block_hash, transaction_hashes) in transaction_hashes_by_block {
        for transaction_hash in transaction_hashes {
            unique_transaction_hashes.insert(transaction_hash);
            let receipt_future = fetch_transaction_receipt_with_retry(
                web3.cheap_clone(),
                *transaction_hash,
                *block_hash,
                logger.cheap_clone(),
            );
            receipt_futures.push(receipt_future)
        }
    }

    // Execute futures while monitoring elapsed time
    let start = Instant::now();
    let receipts: Vec<_> = match receipt_futures.try_collect().await {
        Ok(receipts) => {
            let elapsed = start.elapsed().as_secs_f64();
            subgraph_metrics.observe_request(
                elapsed,
                "eth_getTransactionReceipt",
                &adapter.provider,
            );
            receipts
        }
        Err(ingestor_error) => {
            subgraph_metrics.add_error("eth_getTransactionReceipt", &adapter.provider);
            debug!(
                logger,
                "Error querying transaction receipts: {}", ingestor_error
            );
            return Err(ingestor_error.into());
        }
    };

    // Build a map between transaction hashes and their receipts
    for receipt in receipts.into_iter() {
        if !unique_transaction_hashes.remove(&receipt.transaction_hash) {
            bail!("Received a receipt for a different transaction hash")
        }
        if let Vacant(entry) = receipts_by_hash.entry(receipt.transaction_hash) {
            entry.insert(receipt);
        } else {
            bail!("Received a duplicate transaction receipt")
        }
    }

    // Confidence check: all unique hashes should have been used
    ensure!(
        unique_transaction_hashes.is_empty(),
        "Didn't receive all necessary transaction receipts"
    );
    info!(logger, "RCP: {:?}", receipts_by_hash);

    Ok(receipts_by_hash)
}

fn b256_to_h256(in_data: B256) -> H256 {
    H256(in_data.as_slice()[0..32].try_into().unwrap())
}
fn b64_to_h64(in_data: B64) -> H64 {
    H64(in_data.as_slice()[0..8].try_into().unwrap())
}
fn u256_to_u256(in_data: alloy::primitives::U256) -> web3::types::U256 {
    let u1 = u64::from_le_bytes(in_data.as_le_slice()[0..8].try_into().unwrap_or_default());
    let u2 = u64::from_le_bytes(in_data.as_le_slice()[8..16].try_into().unwrap_or_default());
    let u3 = u64::from_le_bytes(in_data.as_le_slice()[16..24].try_into().unwrap_or_default());
    let u4 = u64::from_le_bytes(in_data.as_le_slice()[24..32].try_into().unwrap_or_default());
    U256([u1, u2, u3, u4])
}
fn u128_to_u256(in_data: u128) -> web3::types::U256 {
    let u1 = (in_data & 0xffffffffffffffff) as u64;
    let u2 = (in_data >> 64) as u64;
    U256([u1, u2, 0, 0])
}
fn u64_to_u256(in_data: u64) -> web3::types::U256 {
    web3::types::U256([in_data, 0, 0, 0])
}
fn u64_to_u64(in_data: u64) -> web3::types::U64 {
    web3::types::U64([in_data])
}
fn bool_to_u64(in_data: bool) -> web3::types::U64 {
    web3::types::U64([if in_data { 1 } else { 0 }])
}
fn u128_to_u64(in_data: u128) -> web3::types::U64 {
    web3::types::U64([(in_data & 0xffffffffffffffff) as u64])
}
fn address_to_h160(fixed_bytes: alloy::primitives::Address) -> H160 {
    let address = H160(fixed_bytes.as_slice().try_into().unwrap());
    address
}
fn h160_to_address(fixed_bytes: &H160) -> alloy::primitives::Address {
    let address =
        alloy::primitives::Address::new(fixed_bytes.as_bytes()[0..20].try_into().unwrap());
    address
}
fn h256_to_b256(fixed_bytes: &H256) -> B256 {
    let bytes = fixed_bytes.as_bytes()[0..32].try_into().unwrap();
    bytes
}
fn convert_bloom(logs_bloom: &alloy::primitives::Bloom) -> H2048 {
    let bytes: [u8; 256] = logs_bloom.as_slice()[0..256].try_into().unwrap();
    H2048::from(bytes)
}
fn bytes_to_bytes(in_data: alloy::primitives::Bytes) -> Bytes {
    let slice = in_data.iter().as_slice();
    Bytes::from(slice)
}

fn convert_topic(
    h256s: &Option<Vec<H256>>,
) -> alloy_rpc_types::FilterSet<alloy::primitives::FixedBytes<32>> {
    if let Some(topic1) = h256s {
        topic1.into_iter().map(|b| h256_to_b256(&b)).collect()
    } else {
        alloy_rpc_types::Topic::default()
    }
}

fn convert_log(alloy_logs: &[alloy_rpc_types::Log<alloy::primitives::LogData>]) -> Vec<Log> {
    alloy_logs
        .iter()
        .map(|log| {
            let address = address_to_h160(log.inner.address);
            let topics = log.topics().iter().map(|t| b256_to_h256(*t)).collect();
            let data = log.inner.data.data.clone().into();
            let block_hash = log.block_hash.map(b256_to_h256);
            let block_number = log.block_number.map(u64_to_u64);
            let transaction_hash = log.transaction_hash.map(b256_to_h256);
            let transaction_index = log.transaction_index.map(u64_to_u64);
            let log_index = log.log_index.map(u64_to_u256);
            let transaction_log_index = None; // TODO: fix it
            let log_type = None; // TODO: fix it
            let removed = Some(log.removed);
            Log {
                address,
                topics,
                data,
                block_hash,
                block_number,
                transaction_hash,
                transaction_index,
                log_index,
                transaction_log_index,
                log_type,
                removed,
            }
        })
        .collect()
}

fn convert_receipts(
    receipts_option: Option<Vec<alloy_rpc_types::TransactionReceipt>>,
) -> Option<Vec<TransactionReceipt>> {
    receipts_option.map(|receipts| {
        receipts
            .into_iter()
            .map(|receipt| {
                let transaction_hash = b256_to_h256(receipt.transaction_hash);
                let transaction_index = u64_to_u64(receipt.transaction_index.unwrap());
                let block_hash = receipt.block_hash.map(b256_to_h256);
                let block_number = receipt.block_number.map(u64_to_u64);
                let from = address_to_h160(receipt.from);
                let to = receipt.to.map(address_to_h160);
                let cumulative_gas_used = u64_to_u256(receipt.blob_gas_used.unwrap_or_default()); // TODO: fix
                let gas_used = Some(u64_to_u256(receipt.gas_used));
                let contract_address = receipt.contract_address.map(address_to_h160);
                let logs = convert_log(receipt.logs());
                let status = Some(bool_to_u64(receipt.status()));
                let root = None; // TODO: fix it
                let logs_bloom = convert_bloom(receipt.inner.logs_bloom());
                let transaction_type = Some(u64_to_u64(0)); // TODO fix it
                let effective_gas_price = Some(u128_to_u256(receipt.effective_gas_price));

                TransactionReceipt {
                    transaction_hash,
                    transaction_index,
                    block_hash,
                    block_number,
                    from,
                    to,
                    cumulative_gas_used,
                    gas_used,
                    contract_address,
                    logs,
                    status,
                    root,
                    logs_bloom,
                    transaction_type,
                    effective_gas_price,
                }
            })
            .collect()
    })
}

fn tx_to_tx(
    logger: &Logger,
    in_data: BlockTransactions<alloy_rpc_types::Transaction>,
) -> Vec<web3::types::Transaction> {
    let _ = logger;
    match in_data {
        BlockTransactions::Full(items) => {
            // if items.len() > 0 {
            //     info!(logger, "ITEMS: {}", items.len());
            // }
            let ret = items
                .iter()
                .map(|tx| -> Transaction {
                    // info!(logger, "TX: {:?}", tx);
                    let inner = tx.inner.inner();
                    let hash = b256_to_h256(inner.hash().clone());
                    let block_hash = tx.block_hash.map(b256_to_h256);
                    let block_number = tx.block_number.map(u64_to_u64);
                    let transaction_index = tx.transaction_index.map(u64_to_u64);
                    let from = Some(address_to_h160(tx.inner.signer()));

                    let gas_price = tx.effective_gas_price.map(u128_to_u256);
                    let raw = None; // TODO: fix it
                    match inner {
                        alloy::consensus::EthereumTxEnvelope::Legacy(signed) => {
                            // info!(logger, "TX legacy: {:?}", signed.tx());
                            // info!(logger, "SIG legacy: {:?}", signed.signature());
                            let nonce = u64_to_u256(signed.tx().nonce);
                            let to = if let alloy::primitives::TxKind::Call(to) = signed.tx().to {
                                Some(address_to_h160(to))
                            } else {
                                None
                            };
                            let value = u256_to_u256(signed.tx().value);
                            let gas = u64_to_u256(signed.tx().gas_limit);
                            let input: web3::types::Bytes = signed.tx().input.clone().into();
                            // let v: Option<web3::types::U64> =
                            //     Some(if signed.signature().v() { 1 } else { 0 }.into());
                            let r = Some(u256_to_u256(signed.signature().r()));
                            let s = Some(u256_to_u256(signed.signature().s()));

                            let v_val =
                                u128_to_u64(alloy::consensus::transaction::to_eip155_value(
                                    signed.signature().v(),
                                    signed.tx().chain_id,
                                ));
                            // info!(
                            //     logger,
                            //     "V_VAL: {:?} LEGACY #{:?} NOT USED", v_val, block_number
                            // );
                            let v = Some(v_val);
                            let transaction_type: Option<web3::types::U64> = Some(0.into()); // TODO: fix it
                            let access_list = None; // TODO: fix it
                            let max_fee_per_gas = None; // TODO: fix it
                            let max_priority_fee_per_gas = None; // TODO: fix it

                            web3::types::Transaction {
                                hash,
                                nonce,
                                block_hash,
                                block_number,
                                transaction_index,
                                from,
                                to,
                                value,
                                gas_price,
                                gas,
                                input,
                                v,
                                r,
                                s,
                                raw,
                                transaction_type,
                                access_list,
                                max_fee_per_gas,
                                max_priority_fee_per_gas,
                            }
                        }
                        alloy::consensus::EthereumTxEnvelope::Eip2930(signed) => {
                            let nonce = u64_to_u256(signed.tx().nonce());
                            let to = if let Some(to) = signed.tx().to() {
                                Some(address_to_h160(to))
                            } else {
                                None
                            };
                            let value = u256_to_u256(signed.tx().value());
                            let gas = u64_to_u256(signed.tx().gas_limit());
                            let input: web3::types::Bytes = signed.tx().input().clone().into();
                            let r = Some(u256_to_u256(signed.signature().r()));
                            let s = Some(u256_to_u256(signed.signature().s()));
                            let v_val =
                                u128_to_u64(alloy::consensus::transaction::to_eip155_value(
                                    signed.signature().v(),
                                    Some(signed.tx().chain_id().unwrap()),
                                ));
                            let v = Some(v_val);
                            let transaction_type: Option<web3::types::U64> = Some(2.into()); // TODO: fix it
                            let access_list = Some(vec![]); // TODO: fix it
                            let max_fee_per_gas = Some(u128_to_u256(signed.tx().max_fee_per_gas()));
                            let max_priority_fee_per_gas =
                                signed.tx().max_priority_fee_per_gas().map(u128_to_u256);
                            web3::types::Transaction {
                                hash,
                                nonce,
                                block_hash,
                                block_number,
                                transaction_index,
                                from,
                                to,
                                value,
                                gas_price,
                                gas,
                                input,
                                v,
                                r,
                                s,
                                raw,
                                transaction_type,
                                access_list,
                                max_fee_per_gas,
                                max_priority_fee_per_gas,
                            }
                        }
                        alloy::consensus::EthereumTxEnvelope::Eip1559(signed) => {
                            // info!(logger, "TX eip1559: {:?}", signed.tx());
                            let nonce = u64_to_u256(signed.tx().nonce);
                            let to = if let alloy::primitives::TxKind::Call(to) = signed.tx().to {
                                Some(address_to_h160(to))
                            } else {
                                None
                            };
                            let value = u256_to_u256(signed.tx().value);
                            let gas = u64_to_u256(signed.tx().gas_limit);
                            let input: web3::types::Bytes = signed.tx().input.clone().into();
                            // let v: Option<web3::types::U64> =
                            //     Some(if signed.signature().v() { 1 } else { 0 }.into());
                            let r = Some(u256_to_u256(signed.signature().r()));
                            let s = Some(u256_to_u256(signed.signature().s()));
                            let v_val =
                                u128_to_u64(alloy::consensus::transaction::to_eip155_value(
                                    signed.signature().v(),
                                    Some(signed.tx().chain_id),
                                ));
                            // info!(logger, "V_VAL: {:?} EIP #{:?}", v_val, block_number);
                            let v = Some(v_val);

                            let transaction_type: Option<web3::types::U64> = Some(2.into()); // TODO: fix it
                            let access_list = Some(vec![]); // TODO: fix it
                            let max_fee_per_gas = Some(u128_to_u256(signed.tx().max_fee_per_gas));
                            let max_priority_fee_per_gas =
                                Some(u128_to_u256(signed.tx().max_priority_fee_per_gas));

                            web3::types::Transaction {
                                hash,
                                nonce,
                                block_hash,
                                block_number,
                                transaction_index,
                                from,
                                to,
                                value,
                                gas_price,
                                gas,
                                input,
                                v,
                                r,
                                s,
                                raw,
                                transaction_type,
                                access_list,
                                max_fee_per_gas,
                                max_priority_fee_per_gas,
                            }
                        }
                        alloy::consensus::EthereumTxEnvelope::Eip4844(signed) => {
                            let nonce = u64_to_u256(signed.tx().nonce());
                            let to = if let Some(to) = signed.tx().to() {
                                Some(address_to_h160(to))
                            } else {
                                None
                            };
                            let value = u256_to_u256(signed.tx().value());
                            let gas = u64_to_u256(signed.tx().gas_limit());
                            let input: web3::types::Bytes = signed.tx().input().clone().into();
                            let r = Some(u256_to_u256(signed.signature().r()));
                            let s = Some(u256_to_u256(signed.signature().s()));
                            let v_val =
                                u128_to_u64(alloy::consensus::transaction::to_eip155_value(
                                    signed.signature().v(),
                                    Some(signed.tx().chain_id().unwrap()),
                                ));
                            let v = Some(v_val);
                            let transaction_type: Option<web3::types::U64> = Some(2.into()); // TODO: fix it
                            let access_list = Some(vec![]); // TODO: fix it
                            let max_fee_per_gas = Some(u128_to_u256(signed.tx().max_fee_per_gas()));
                            let max_priority_fee_per_gas = Some(u128_to_u256(
                                signed.tx().max_priority_fee_per_gas().unwrap(),
                            ));
                            web3::types::Transaction {
                                hash,
                                nonce,
                                block_hash,
                                block_number,
                                transaction_index,
                                from,
                                to,
                                value,
                                gas_price,
                                gas,
                                input,
                                v,
                                r,
                                s,
                                raw,
                                transaction_type,
                                access_list,
                                max_fee_per_gas,
                                max_priority_fee_per_gas,
                            }
                        }
                        alloy::consensus::EthereumTxEnvelope::Eip7702(signed) => {
                            let nonce = u64_to_u256(signed.tx().nonce());
                            let to = if let Some(to) = signed.tx().to() {
                                Some(address_to_h160(to))
                            } else {
                                None
                            };
                            let value = u256_to_u256(signed.tx().value());
                            let gas = u64_to_u256(signed.tx().gas_limit());
                            let input: web3::types::Bytes = signed.tx().input().clone().into();
                            let r = Some(u256_to_u256(signed.signature().r()));
                            let s = Some(u256_to_u256(signed.signature().s()));
                            let v_val =
                                u128_to_u64(alloy::consensus::transaction::to_eip155_value(
                                    signed.signature().v(),
                                    Some(signed.tx().chain_id().unwrap()),
                                ));
                            let v = Some(v_val);
                            let transaction_type: Option<web3::types::U64> = Some(2.into()); // TODO: fix it
                            let access_list = Some(vec![]); // TODO: fix it
                            let max_fee_per_gas = Some(u128_to_u256(signed.tx().max_fee_per_gas()));
                            let max_priority_fee_per_gas = Some(u128_to_u256(
                                signed.tx().max_priority_fee_per_gas().unwrap(),
                            ));
                            web3::types::Transaction {
                                hash,
                                nonce,
                                block_hash,
                                block_number,
                                transaction_index,
                                from,
                                to,
                                value,
                                gas_price,
                                gas,
                                input,
                                v,
                                r,
                                s,
                                raw,
                                transaction_type,
                                access_list,
                                max_fee_per_gas,
                                max_priority_fee_per_gas,
                            }
                        }
                    }
                })
                .collect::<Vec<_>>();
            // info!(logger, "DONE WITH ITEMS");
            ret
        }
        BlockTransactions::Hashes(_items) => {
            // items
            //     .iter()
            //     .map(|hash| b256_to_h256(*hash))
            //     .collect::<Vec<_>>();
            panic!("Not implemented variant: Hashes");
        }
        BlockTransactions::Uncle => panic!("Not implemented variant: Uncle"),
    }
}

fn convert_block_alloy2web3(
    logger: &Logger,
    block: alloy_rpc_types::Block,
) -> Arc<LightEthereumBlock> {
    let hash = Some(b256_to_h256(block.header.hash));
    // info!(logger, "hash: {:?}", hash);
    let parent_hash = b256_to_h256(block.header.inner.parent_hash);
    // info!(logger, "parent_hash: {:?}", parent_hash);
    let uncles_hash = b256_to_h256(block.header.inner.ommers_hash);
    // info!(logger, "uncles_hash: {:?}", uncles_hash);
    let author = address_to_h160(block.header.inner.beneficiary);
    // info!(logger, "author: {:?}", author);
    let state_root = b256_to_h256(block.header.state_root);
    // info!(logger, "state_root: {:?}", state_root);
    let transactions_root = b256_to_h256(block.header.transactions_root);
    // info!(logger, "transactions_root: {:?}", transactions_root);
    let receipts_root = b256_to_h256(block.header.receipts_root);
    // info!(logger, "receipts_root: {:?}", receipts_root);
    let number = Some(web3::types::U64([block.header.number; 1]));
    // info!(logger, "number: {:?}", number);
    let gas_used = u64_to_u256(block.header.gas_used);
    // info!(logger, "gas_used: {:?}", gas_used);
    let gas_limit = u64_to_u256(block.header.gas_limit);
    // info!(logger, "gas_limit: {:?}", gas_limit);
    let base_fee_per_gas = block.header.base_fee_per_gas.map(|n| u64_to_u256(n));
    // info!(logger, "base_fee_per_gas: {:?}", base_fee_per_gas);
    let extra_data = web3::types::Bytes(block.header.extra_data.to_vec());
    // info!(logger, "extra_data: {:?}", extra_data);
    let logs_bloom = Some(H2048(
        block.header.inner.logs_bloom.as_slice()[0..256]
            .try_into()
            .unwrap(),
    ));
    // info!(logger, "logs_bloom: {:?}", logs_bloom);
    let timestamp = u64_to_u256(block.header.inner.timestamp);
    // info!(logger, "timestamp: {:?}", timestamp);
    let difficulty = u256_to_u256(block.header.inner.difficulty);
    // info!(logger, "difficulty: {:?}", difficulty);
    let total_difficulty = block.header.total_difficulty.map(|n| u256_to_u256(n));
    // info!(logger, "total_difficulty: {:?}", total_difficulty);
    let uncles = block.uncles.into_iter().map(|h| b256_to_h256(h)).collect();
    // info!(logger, "uncles: {:?}", uncles);
    let transactions = tx_to_tx(logger, block.transactions);
    let size = block.header.size.map(|n| u256_to_u256(n));
    // info!(logger, "size: {:?}", size);
    let mix_hash = Some(b256_to_h256(block.header.mix_hash));
    // info!(logger, "mix_hash: {:?}", mix_hash);
    let nonce = Some(b64_to_h64(block.header.nonce));
    // info!(logger, "nonce: {:?}", nonce);
    let light_block = LightEthereumBlock {
        hash,
        parent_hash,
        uncles_hash,
        author,
        state_root,
        transactions_root,
        receipts_root,
        number,
        gas_used,
        gas_limit,
        base_fee_per_gas,
        extra_data,
        logs_bloom,
        timestamp,
        difficulty,
        total_difficulty,
        seal_fields: vec![], // TODO: fix this
        uncles,
        transactions,
        size,
        mix_hash,
        nonce,
    };
    // info!(logger, "light_block: {:?}", light_block);
    Arc::new(light_block)
}

fn tx_hash_to_tx(
    logger: &Logger,
    in_data: BlockTransactions<alloy_rpc_types::Transaction>,
) -> Vec<H256> {
    let _ = logger;
    match in_data {
        BlockTransactions::Full(_) => panic!("Wrong variant: Full"),
        BlockTransactions::Hashes(items) => {
            let v = items
                .iter()
                .map(|hash| {
                    // info!(logger, "HASH: {:?}", hash);
                    b256_to_h256(*hash)
                })
                .collect::<Vec<_>>();
            v
        }
        BlockTransactions::Uncle => panic!("Not implemented variant: Uncle"),
    }
}

fn convert_block_hash_alloy2web3(
    logger: &Logger,
    block: alloy_rpc_types::Block,
) -> Arc<web3::types::Block<H256>> {
    let hash = Some(b256_to_h256(block.header.hash));
    // info!(logger, "hash: {:?}", hash);
    let parent_hash = b256_to_h256(block.header.inner.parent_hash);
    // info!(logger, "parent_hash: {:?}", parent_hash);
    let uncles_hash = b256_to_h256(block.header.inner.ommers_hash);
    // info!(logger, "uncles_hash: {:?}", uncles_hash);
    let author = address_to_h160(block.header.inner.beneficiary);
    // info!(logger, "author: {:?}", author);
    let state_root = b256_to_h256(block.header.state_root);
    // info!(logger, "state_root: {:?}", state_root);
    let transactions_root = b256_to_h256(block.header.transactions_root);
    // info!(logger, "transactions_root: {:?}", transactions_root);
    let receipts_root = b256_to_h256(block.header.receipts_root);
    // info!(logger, "receipts_root: {:?}", receipts_root);
    let number = Some(web3::types::U64([block.header.number; 1]));
    // info!(logger, "number: {:?}", number);
    let gas_used = u64_to_u256(block.header.gas_used);
    // info!(logger, "gas_used: {:?}", gas_used);
    let gas_limit = u64_to_u256(block.header.gas_limit);
    // info!(logger, "gas_limit: {:?}", gas_limit);
    let base_fee_per_gas = block.header.base_fee_per_gas.map(|n| u64_to_u256(n));
    // info!(logger, "base_fee_per_gas: {:?}", base_fee_per_gas);
    let extra_data = web3::types::Bytes(block.header.extra_data.to_vec());
    // info!(logger, "extra_data: {:?}", extra_data);
    let logs_bloom = Some(H2048(
        block.header.inner.logs_bloom.as_slice()[0..256]
            .try_into()
            .unwrap(),
    ));
    // info!(logger, "logs_bloom: {:?}", logs_bloom);
    let timestamp = u64_to_u256(block.header.inner.timestamp);
    // info!(logger, "timestamp: {:?}", timestamp);
    let difficulty = u256_to_u256(block.header.inner.difficulty);
    // info!(logger, "difficulty: {:?}", difficulty);
    let total_difficulty = block.header.total_difficulty.map(|n| u256_to_u256(n));
    // info!(logger, "total_difficulty: {:?}", total_difficulty);
    let uncles = block.uncles.into_iter().map(|h| b256_to_h256(h)).collect();
    // info!(logger, "uncles: {:?}", uncles);
    let transactions = tx_hash_to_tx(logger, block.transactions);
    // let transactions = block.transactions.map(|hash|);
    let size = block.header.size.map(|n| u256_to_u256(n));
    // info!(logger, "size: {:?}", size);
    let mix_hash = Some(b256_to_h256(block.header.mix_hash));
    // info!(logger, "mix_hash: {:?}", mix_hash);
    let nonce = Some(b64_to_h64(block.header.nonce));
    // info!(logger, "nonce: {:?}", nonce);
    let block = web3::types::Block::<H256> {
        hash,
        parent_hash,
        uncles_hash,
        author,
        state_root,
        transactions_root,
        receipts_root,
        number,
        gas_used,
        gas_limit,
        base_fee_per_gas,
        extra_data,
        logs_bloom,
        timestamp,
        difficulty,
        total_difficulty,
        seal_fields: vec![], // TODO: fix this
        uncles,
        transactions,
        size,
        mix_hash,
        nonce,
    };
    Arc::new(block)
}

fn semi_equal(
    logger: &Logger,
    block1: &web3::types::Block<Transaction>,
    block2: &web3::types::Block<Transaction>,
) -> bool {
    if block1.transactions.len() != block2.transactions.len() {
        info!(
            logger,
            "different TX sizes: {} vs {}",
            block1.transactions.len(),
            block2.transactions.len()
        );
        return false;
    }
    for i in 0..block1.transactions.len() {
        let mut tx1 = block1.transactions[i].clone();
        tx1.v = None;
        tx1.transaction_type = None;
        tx1.access_list = None;
        tx1.max_fee_per_gas = None;
        let mut tx2 = block2.transactions[i].clone();
        tx2.v = None;
        tx2.transaction_type = None;
        tx2.access_list = None;
        tx2.max_fee_per_gas = None;
        if tx1 != tx2 {
            info!(logger, "different TX (block #{:?}):", block1.number);
            info!(logger, "TX1: {:?}", tx1);
            info!(logger, "TX2: {:?}", tx2);
            return false;
        }
    }
    let mut bl1 = block1.clone();
    bl1.transactions = vec![];
    let mut bl2 = block2.clone();
    bl2.transactions = vec![];
    if bl1 != bl2 {
        info!(logger, "different BL (block #{:?}):", block1.number);
        info!(logger, "BL1: {:?}", bl1);
        info!(logger, "BL2: {:?}", bl2);
        return false;
    }
    true
}

#[cfg(test)]
mod tests {

    use crate::trigger::{EthereumBlockTriggerType, EthereumTrigger};

    use super::{
        check_block_receipt_support, parse_block_triggers, EthereumBlock, EthereumBlockFilter,
        EthereumBlockWithCalls,
    };
    use graph::blockchain::BlockPtr;
    use graph::prelude::tokio::{self};
    use graph::prelude::web3::transports::test::TestTransport;
    use graph::prelude::web3::types::U64;
    use graph::prelude::web3::types::{Address, Block, Bytes, H256};
    use graph::prelude::web3::Web3;
    use graph::prelude::EthereumCall;
    use jsonrpc_core::serde_json::{self, Value};
    use std::collections::HashSet;
    use std::iter::FromIterator;
    use std::sync::Arc;

    #[test]
    fn parse_block_triggers_every_block() {
        let block = EthereumBlockWithCalls {
            ethereum_block: EthereumBlock {
                block: Arc::new(Block {
                    hash: Some(hash(2)),
                    number: Some(U64::from(2)),
                    ..Default::default()
                }),
                ..Default::default()
            },
            calls: Some(vec![EthereumCall {
                to: address(4),
                input: bytes(vec![1; 36]),
                ..Default::default()
            }]),
        };

        assert_eq!(
            vec![
                EthereumTrigger::Block(
                    BlockPtr::from((hash(2), 2)),
                    EthereumBlockTriggerType::Start
                ),
                EthereumTrigger::Block(BlockPtr::from((hash(2), 2)), EthereumBlockTriggerType::End)
            ],
            parse_block_triggers(
                &EthereumBlockFilter {
                    polling_intervals: HashSet::new(),
                    contract_addresses: HashSet::from_iter(vec![(10, address(1))]),
                    trigger_every_block: true,
                },
                &block
            ),
            "every block should generate a trigger even when address don't match"
        );
    }

    #[tokio::test]
    async fn test_check_block_receipts_support() {
        let mut transport = TestTransport::default();

        let json_receipts = r#"[{
            "blockHash": "0x23f785604642e91613881fc3c9d16740ee416e340fd36f3fa2239f203d68fd33",
            "blockNumber": "0x12f7f81",
            "contractAddress": null,
            "cumulativeGasUsed": "0x26f66",
            "effectiveGasPrice": "0x140a1bd03",
            "from": "0x56fc0708725a65ebb633efdaec931c0600a9face",
            "gasUsed": "0x26f66",
            "logs": [],
            "logsBloom": "0x00000000010000000000000000000000000000000000000000000000040000000000000000000000000008000000000002000000080020000000040000000000000000000000000808000008000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000010000800000000000000000000000000000000000000000000010000000000000000000000000000000000200000000000000000000000000000000000002000000008000000000002000000000000000000000000000000000400000000000000000000000000200000000000000010000000000000000000000000000000000000000000",
            "status": "0x1",
            "to": "0x51c72848c68a965f66fa7a88855f9f7784502a7f",
            "transactionHash": "0xabfe9e82d71c843a91251fd1272b0dd80bc0b8d94661e3a42c7bb9e7f55789cf",
            "transactionIndex": "0x0",
            "type": "0x2"
        }]"#;

        let json_empty = r#"[]"#;

        // Helper function to run a single test case
        async fn run_test_case(
            transport: &mut TestTransport,
            json_response: &str,
            expected_err: Option<&str>,
            supports_eip_1898: bool,
            call_only: bool,
        ) -> Result<(), anyhow::Error> {
            let json_value: Value = serde_json::from_str(json_response).unwrap();
            // let block_json: Value = serde_json::from_str(block).unwrap();
            transport.set_response(json_value);
            // transport.set_response(block_json);
            // transport.add_response(json_value);

            let web3 = Arc::new(Web3::new(transport.clone()));
            let asserter = alloy::transports::mock::Asserter::new();
            // asserter.push(json_value);
            let alloy =
                Arc::new(alloy::providers::ProviderBuilder::new().connect_mocked_client(asserter));
            let result = check_block_receipt_support(
                alloy,
                web3.clone(),
                H256::zero(),
                supports_eip_1898,
                call_only,
            )
            .await;

            match expected_err {
                Some(err_msg) => match result {
                    Ok(_) => panic!("Expected error but got Ok"),
                    Err(e) => {
                        assert!(e.to_string().contains(err_msg));
                    }
                },
                None => match result {
                    Ok(_) => (),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        panic!("Unexpected error: {}", e);
                    }
                },
            }
            Ok(())
        }

        // Test case 1: Valid block receipts
        run_test_case(&mut transport, json_receipts, None, true, false)
            .await
            .unwrap();

        // Test case 2: Empty block receipts
        run_test_case(
            &mut transport,
            json_empty,
            Some("Block receipts are empty"),
            true,
            false,
        )
        .await
        .unwrap();

        // Test case 3: Null response
        run_test_case(
            &mut transport,
            "null",
            Some("Block receipts are empty"),
            true,
            false,
        )
        .await
        .unwrap();

        // Test case 3: Simulating an RPC error
        // Note: In the context of this test, we cannot directly simulate an RPC error.
        // Instead, we simulate a response that would cause a decoding error, such as an unexpected key("error").
        // The function should handle this as an error case.
        run_test_case(
            &mut transport,
            r#"{"error":"RPC Error"}"#,
            Some("Error fetching block receipts:"),
            true,
            false,
        )
        .await
        .unwrap();

        // Test case 5: Does not support EIP-1898
        run_test_case(
            &mut transport,
            json_receipts,
            Some("Provider does not support EIP 1898"),
            false,
            false,
        )
        .await
        .unwrap();

        // Test case 5: Does not support Call only adapters
        run_test_case(
            &mut transport,
            json_receipts,
            Some("Provider is call-only"),
            true,
            true,
        )
        .await
        .unwrap();
    }

    #[test]
    fn parse_block_triggers_specific_call_not_found() {
        let block = EthereumBlockWithCalls {
            ethereum_block: EthereumBlock {
                block: Arc::new(Block {
                    hash: Some(hash(2)),
                    number: Some(U64::from(2)),
                    ..Default::default()
                }),
                ..Default::default()
            },
            calls: Some(vec![EthereumCall {
                to: address(4),
                input: bytes(vec![1; 36]),
                ..Default::default()
            }]),
        };

        assert_eq!(
            Vec::<EthereumTrigger>::new(),
            parse_block_triggers(
                &EthereumBlockFilter {
                    polling_intervals: HashSet::new(),
                    contract_addresses: HashSet::from_iter(vec![(1, address(1))]),
                    trigger_every_block: false,
                },
                &block
            ),
            "block filter specifies address 1 but block does not contain any call to it"
        );
    }

    #[test]
    fn parse_block_triggers_specific_call_found() {
        let block = EthereumBlockWithCalls {
            ethereum_block: EthereumBlock {
                block: Arc::new(Block {
                    hash: Some(hash(2)),
                    number: Some(U64::from(2)),
                    ..Default::default()
                }),
                ..Default::default()
            },
            calls: Some(vec![EthereumCall {
                to: address(4),
                input: bytes(vec![1; 36]),
                ..Default::default()
            }]),
        };

        assert_eq!(
            vec![EthereumTrigger::Block(
                BlockPtr::from((hash(2), 2)),
                EthereumBlockTriggerType::WithCallTo(address(4))
            )],
            parse_block_triggers(
                &EthereumBlockFilter {
                    polling_intervals: HashSet::new(),
                    contract_addresses: HashSet::from_iter(vec![(1, address(4))]),
                    trigger_every_block: false,
                },
                &block
            ),
            "block filter specifies address 4 and block has call to it"
        );
    }

    fn address(id: u64) -> Address {
        Address::from_low_u64_be(id)
    }

    fn hash(id: u8) -> H256 {
        H256::from([id; 32])
    }

    fn bytes(value: Vec<u8>) -> Bytes {
        Bytes::from(value)
    }
}
