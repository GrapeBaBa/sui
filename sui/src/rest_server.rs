// Copyright (c) Mysten Labs
// SPDX-License-Identifier: Apache-2.0

use http::Response;
use move_core_types::identifier::Identifier;
use move_core_types::parser::parse_type_tag;
use move_core_types::value::MoveStructLayout;
use sui::sui_json::{resolve_move_function_args, SuiJsonValue};

use dropshot::{endpoint, Query, CONTENT_TYPE_JSON};
use dropshot::{
    ApiDescription, ConfigDropshot, ConfigLogging, ConfigLoggingLevel, HttpError, HttpResponseOk,
    HttpResponseUpdatedNoContent, HttpServerStarter, RequestContext, TypedBody,
};
use futures::lock::Mutex;
use hyper::{Body, StatusCode};
use serde_json::json;
use sui::config::{Config, GenesisConfig, NetworkConfig, WalletConfig};
use sui::sui_commands;
use sui::wallet_commands::WalletContext;
use sui_types::move_package::resolve_and_type_check;

use sui_core::client::Client;
use sui_types::committee::Committee;
use sui_types::messages::{ExecutionStatus, TransactionEffects};
use sui_types::object::Object as SuiObject;
use sui_types::{base_types::*, object::ObjectRead};

use futures::stream::{futures_unordered::FuturesUnordered, StreamExt as _};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use tokio::task::{self, JoinHandle};
use tracing::{error, info};

use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_dropshot: ConfigDropshot = ConfigDropshot {
        bind_address: SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 5000)),
        ..Default::default()
    };

    let config_logging = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Info,
    };
    let log = config_logging
        .to_logger("rest_server")
        .map_err(|error| format!("failed to create logger: {}", error))?;

    tracing_subscriber::fmt().init();

    let mut api = ApiDescription::new();

    // [DEBUG]
    api.register(genesis).unwrap();
    api.register(sui_start).unwrap();
    api.register(sui_stop).unwrap();

    // [WALLET]
    api.register(get_addresses).unwrap();
    api.register(get_objects).unwrap();
    api.register(object_info).unwrap();
    api.register(transfer_object).unwrap();
    api.register(publish).unwrap();
    api.register(call).unwrap();
    api.register(sync).unwrap();

    api.openapi("Sui API", "0.1")
        .write(&mut std::io::stdout())
        .map_err(|e| e.to_string())?;

    let api_context = ServerContext::new();

    let server = HttpServerStarter::new(&config_dropshot, api, api_context, &log)
        .map_err(|error| format!("failed to create server: {}", error))?
        .start();

    server.await
}

/**
 * Server context (state shared by handler functions)
 */
struct ServerContext {
    genesis_config_path: String,
    wallet_config_path: String,
    network_config_path: String,
    authority_db_path: String,
    client_db_path: Arc<Mutex<String>>,
    // Server handles that will be used to restart authorities.
    authority_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    // Used to manage addresses for client.
    wallet_context: Arc<Mutex<Option<WalletContext>>>,
}

impl ServerContext {
    pub fn new() -> ServerContext {
        ServerContext {
            genesis_config_path: String::from("genesis.conf"),
            wallet_config_path: String::from("wallet.conf"),
            network_config_path: String::from("./network.conf"),
            authority_db_path: String::from("./authorities_db"),
            client_db_path: Arc::new(Mutex::new(String::new())),
            authority_handles: Arc::new(Mutex::new(Vec::new())),
            wallet_context: Arc::new(Mutex::new(None)),
        }
    }
}

/**
Request containing the server configuration.

All attributes in GenesisRequest are optional, a default value will be used if
the fields are not set.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GenesisRequest {
    custom_genesis: bool,
}

/**
Response containing the resulting wallet & network config of the
provided genesis configuration.
 */
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GenesisResponse {
    /** List of managed addresses and the list of authorities */
    wallet_config: serde_json::Value,
    /** Information about authorities and the list of loaded move packages. */
    network_config: serde_json::Value,
}

/**
Specify the genesis state of the network.

You can specify the number of authorities, an initial number of addresses
and the number of gas objects to be assigned to those addresses.

Note: This is a temporary endpoint that will no longer be needed once the
network has been started on testnet or mainnet.
 */
#[endpoint {
    method = POST,
    path = "/sui/genesis",
    tags = [ "debug" ],
}]
async fn genesis(
    rqctx: Arc<RequestContext<ServerContext>>,
    request: TypedBody<GenesisRequest>,
) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();
    let genesis_request_params = request.into_inner();
    let genesis_config_path = &server_context.genesis_config_path;
    let network_config_path = &server_context.network_config_path;
    let wallet_config_path = &server_context.wallet_config_path;

    let mut network_config = NetworkConfig::read_or_create(&PathBuf::from(network_config_path))
        .map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Unable to read network config: {error}"),
            )
        })?;

    if !network_config.authorities.is_empty() {
        return Err(custom_http_error(
            StatusCode::CONFLICT,
            String::from("Cannot run genesis on a existing network, please make a POST request to the `sui/stop` endpoint to reset."),
        ));
    }

    let working_dir = network_config.config_path().parent().unwrap().to_owned();
    let genesis_conf = if genesis_request_params.custom_genesis {
        GenesisConfig::read(&working_dir.join(genesis_config_path)).map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Unable to read genesis configuration: {error}"),
            )
        })?
    } else {
        GenesisConfig::default_genesis(&working_dir.join(genesis_config_path)).map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Unable to create default genesis configuration: {error}"),
            )
        })?
    };

    // println!("{:#?}", &genesis_conf);

    let wallet_path = working_dir.join(wallet_config_path);
    let mut wallet_config =
        WalletConfig::create(&working_dir.join(wallet_path)).map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Wallet config was unable to be created: {error}"),
            )
        })?;
    // Need to use a random id because rocksdb locks on current process which
    // means even if the directory is deleted the lock will remain causing an
    // IO Error when a restart is attempted.
    let client_db_path = format!("client_db_{:?}", ObjectID::random());
    wallet_config.db_folder_path = working_dir.join(&client_db_path);
    *server_context.client_db_path.lock().await = client_db_path;

    sui_commands::genesis(&mut network_config, genesis_conf, &mut wallet_config)
        .await
        .map_err(|err| {
            custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Genesis error: {:?}", err),
            )
        })?;

    custom_http_response(
        StatusCode::OK,
        GenesisResponse {
            wallet_config: json!(wallet_config),
            network_config: json!(network_config),
        },
    )
    .map_err(|err| custom_http_error(StatusCode::BAD_REQUEST, format!("{err}")))
}

/**
Start servers with the specified configurations from the genesis endpoint.

Note: This is a temporary endpoint that will no longer be needed once the
network has been started on testnet or mainnet.
 */
#[endpoint {
    method = POST,
    path = "/sui/start",
    tags = [ "debug" ],
}]
async fn sui_start(rqctx: Arc<RequestContext<ServerContext>>) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();
    let network_config_path = &server_context.network_config_path;

    let network_config = NetworkConfig::read_or_create(&PathBuf::from(network_config_path))
        .map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Unable to read network config: {error}"),
            )
        })?;

    if network_config.authorities.is_empty() {
        return Err(custom_http_error(
            StatusCode::CONFLICT,
            String::from("No authority configured for the network, please make a POST request to the `sui/genesis` endpoint."),
        ));
    }

    {
        if !(*server_context.authority_handles.lock().await).is_empty() {
            return Err(custom_http_error(
                StatusCode::FORBIDDEN,
                String::from("Sui network is already running."),
            ));
        }
    }

    let committee = Committee::new(
        network_config
            .authorities
            .iter()
            .map(|info| (*info.key_pair.public_key_bytes(), info.stake))
            .collect(),
    );
    let mut handles = FuturesUnordered::new();

    for authority in &network_config.authorities {
        let server = sui_commands::make_server(
            authority,
            &committee,
            vec![],
            &[],
            network_config.buffer_size,
        )
        .await
        .map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Unable to make server: {error}"),
            )
        })?;
        handles.push(async move {
            match server.spawn().await {
                Ok(server) => Ok(server),
                Err(err) => {
                    return Err(custom_http_error(
                        StatusCode::FAILED_DEPENDENCY,
                        format!("Failed to start server: {}", err),
                    ));
                }
            }
        })
    }

    let num_authorities = handles.len();
    info!("Started {} authorities", num_authorities);

    while let Some(spawned_server) = handles.next().await {
        server_context
            .authority_handles
            .lock()
            .await
            .push(task::spawn(async {
                if let Err(err) = spawned_server.unwrap().join().await {
                    error!("Server ended with an error: {}", err);
                }
            }));
    }

    let wallet_config_path = &server_context.wallet_config_path;

    let wallet_config =
        WalletConfig::read_or_create(&PathBuf::from(wallet_config_path)).map_err(|error| {
            custom_http_error(
                StatusCode::CONFLICT,
                format!("Unable to read wallet config: {error}"),
            )
        })?;

    let addresses = wallet_config.accounts.clone();
    let mut wallet_context = WalletContext::new(wallet_config).map_err(|error| {
        custom_http_error(
            StatusCode::CONFLICT,
            format!("Can't create new wallet context: {error}"),
        )
    })?;

    // Sync all accounts.
    for address in addresses.iter() {
        wallet_context
            .address_manager
            .sync_client_state(*address)
            .await
            .map_err(|err| {
                custom_http_error(
                    StatusCode::FAILED_DEPENDENCY,
                    format!("Sync error: {:?}", err),
                )
            })?;
    }

    *server_context.wallet_context.lock().await = Some(wallet_context);

    custom_http_response(
        StatusCode::OK,
        format!("Started {} authorities", num_authorities),
    )
    .map_err(|err| custom_http_error(StatusCode::BAD_REQUEST, format!("{err}")))
}

/**
Stop sui network and delete generated configs & storage.

Note: This is a temporary endpoint that will no longer be needed once the
network has been started on testnet or mainnet.
 */
#[endpoint {
    method = POST,
    path = "/sui/stop",
    tags = [ "debug" ],
}]
async fn sui_stop(
    rqctx: Arc<RequestContext<ServerContext>>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let server_context = rqctx.context();

    for authority_handle in &*server_context.authority_handles.lock().await {
        authority_handle.abort();
    }
    (*server_context.authority_handles.lock().await).clear();

    fs::remove_dir_all(server_context.client_db_path.lock().await.clone()).ok();
    fs::remove_dir_all(&server_context.authority_db_path).ok();
    fs::remove_file(&server_context.network_config_path).ok();
    fs::remove_file(&server_context.wallet_config_path).ok();

    Ok(HttpResponseUpdatedNoContent())
}

/**
Response containing the managed addresses for this client.
 */
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetAddressResponse {
    /** Vector of hex codes as strings representing the managed addresses */
    addresses: Vec<String>,
}

/**
Retrieve all managed addresses for this client.
 */
#[allow(unused_variables)]
#[endpoint {
    method = GET,
    path = "/addresses",
    tags = [ "wallet" ],
}]
async fn get_addresses(
    rqctx: Arc<RequestContext<ServerContext>>,
) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();
    let mut wallet_context = server_context.wallet_context.lock().await;
    let wallet_context = wallet_context.as_mut().ok_or_else(|| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            "Wallet Context does not exist.".to_string(),
        )
    })?;

    let addresses: Vec<SuiAddress> = wallet_context
        .address_manager
        .get_managed_address_states()
        .keys()
        .copied()
        .collect();

    // TODO: Speed up sync operations by kicking them off concurrently.
    // Also need to investigate if this should be an automatic sync or manually triggered.
    for address in addresses.iter() {
        if let Err(err) = wallet_context
            .address_manager
            .sync_client_state(*address)
            .await
        {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Can't create client state: {err}"),
            ));
        }
    }

    custom_http_response(
        StatusCode::OK,
        GetAddressResponse {
            addresses: addresses
                .into_iter()
                .map(|address| format!("{}", address))
                .collect(),
        },
    )
    .map_err(|err| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{err}")))
}

/**
Request containing the address of which objecst are to be retrieved.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetObjectsRequest {
    /** Required; Hex code as string representing the address */
    address: String,
}

/**
JSON representation of an object in the Sui network.
 */
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Object {
    /** Hex code as string representing the object id */
    object_id: String,
    /** Type of object, i.e. Coin */
    obj_type: String,
    /** Object version */
    version: String,
    /** Hash of the object's contents used for local validation */
    object_digest: String,
}

/**
Returns the list of objects owned by an address.
 */
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetObjectsResponse {
    objects: Vec<Object>,
}

/**
Returns list of objects owned by an address.
 */
#[allow(unused_variables)]
#[endpoint {
    method = GET,
    path = "/objects",
    tags = [ "wallet" ],
}]
async fn get_objects(
    rqctx: Arc<RequestContext<ServerContext>>,
    query: Query<GetObjectsRequest>,
) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();

    let get_objects_params = query.into_inner();
    let address = get_objects_params.address;

    let wallet_context = &mut *server_context.wallet_context.lock().await;
    let wallet_context = wallet_context.as_mut().ok_or_else(|| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            "Wallet Context does not exist. Please make a POST request to `sui/genesis/` and `sui/start/` to bootstrap the network."
                .to_string(),
        )
    })?;

    let address = &decode_bytes_hex(address.as_str()).map_err(|error| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Could not decode address from hex {error}"),
        )
    })?;

    let object_refs = wallet_context.address_manager.get_owned_objects(*address);
    let mut objects = vec![];
    for (object_id, sequence_number, object_digest) in object_refs {
        let object = match get_object_info(wallet_context, object_id).await {
            Ok((_, object, _)) => object,
            Err(error) => {
                return Err(error);
            }
        };
        let obj_type = object
            .data
            .type_()
            .map_or("Unknown Type".to_owned(), |type_| format!("{}", type_));

        objects.push(Object {
            object_id: object_id.to_string(),
            obj_type,
            version: format!("{:?}", sequence_number),
            object_digest: format!("{:?}", object_digest),
        });
    }

    custom_http_response(StatusCode::OK, objects)
        .map_err(|err| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{err}")))
}

/**
Request containing the object for which info is to be retrieved.

If owner is specified we look for this object in that address's account store,
otherwise we look for it in the shared object store.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetObjectInfoRequest {
    /** Required; Hex code as string representing the object id */
    object_id: String,
}

/**
Response containing the information of an object if found, otherwise an error
is returned.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ObjectInfoResponse {
    /** Hex code as string representing the owner's address */
    owner: String,
    /** Sequence number of the object */
    version: String,
    /** Hex code as string representing the objet id */
    id: String,
    /** Boolean representing if the object is mutable */
    readonly: String,
    /** Type of object, i.e. Coin */
    obj_type: String,
    /** JSON representation of the object data */
    data: serde_json::Value,
}

/**
Returns the object information for a specified object.
 */
#[allow(unused_variables)]
#[endpoint {
    method = GET,
    path = "/object_info",
    tags = [ "wallet" ],
}]
async fn object_info(
    rqctx: Arc<RequestContext<ServerContext>>,
    query: Query<GetObjectInfoRequest>,
) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();
    let object_info_params = query.into_inner();

    let mut wallet_context = server_context.wallet_context.lock().await;
    let wallet_context = wallet_context.as_mut().ok_or_else(|| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            "Wallet Context does not exist. Please make a POST request to `sui/genesis/` and `sui/start/` to bootstrap the network."
                .to_string(),
        )
    })?;

    let object_id = match ObjectID::try_from(object_info_params.object_id) {
        Ok(object_id) => object_id,
        Err(error) => {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("{error}"),
            ));
        }
    };

    let (object, layout) = match get_object_info(wallet_context, object_id).await {
        Ok((_, object, layout)) => (object, layout),
        Err(error) => {
            return Err(error);
        }
    };

    let object_data = object.to_json(&layout).unwrap_or_else(|_| json!(""));

    custom_http_response(
        StatusCode::OK,
        &ObjectInfoResponse {
            owner: format!("{:?}", object.owner),
            version: format!("{:?}", object.version().value()),
            id: format!("{:?}", object.id()),
            readonly: format!("{:?}", object.is_read_only()),
            obj_type: object
                .data
                .type_()
                .map_or("Unknown Type".to_owned(), |type_| format!("{}", type_)),
            data: object_data,
        },
    )
    .map_err(|err| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{err}")))
}

/**
Request containing the information needed to execute a transfer transaction.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct TransferTransactionRequest {
    /** Required; Hex code as string representing the address to be sent from */
    from_address: String,
    /** Required; Hex code as string representing the object id */
    object_id: String,
    /** Required; Hex code as string representing the address to be sent to */
    to_address: String,
    /** Required; Hex code as string representing the gas object id to be used as payment */
    gas_object_id: String,
}

/**
Response containing the summary of effects made on an object and the certificate
associated with the transaction that verifies the transaction.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct TransactionResponse {
    /** Integer representing the acutal cost of the transaction */
    gas_used: u64,
    /** JSON representation of the list of resulting effects on the object */
    object_effects_summary: serde_json::Value,
    /** JSON representation of the certificate verifying the transaction */
    certificate: serde_json::Value,
}

/**
Transfer object from one address to another. Gas will be paid using the gas
provided in the request. This will be done through a native transfer
transaction that does not require Move VM executions, hence is much cheaper.

Notes:
- Non-coin objects cannot be transferred natively and will require a Move call

Example TransferTransactionRequest
{
    "from_address": "1DA89C9279E5199DDC9BC183EB523CF478AB7168",
    "object_id": "4EED236612B000B9BEBB99BA7A317EFF27556A0C",
    "to_address": "5C20B3F832F2A36ED19F792106EC73811CB5F62C",
    "gas_object_id": "96ABE602707B343B571AAAA23E3A4594934159A5"
}
 */
#[endpoint {
    method = POST,
    path = "/transfer",
    tags = [ "wallet" ],
}]
async fn transfer_object(
    rqctx: Arc<RequestContext<ServerContext>>,
    request: TypedBody<TransferTransactionRequest>,
) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();
    let transfer_order_params = request.into_inner();
    let to_address =
        decode_bytes_hex(transfer_order_params.to_address.as_str()).map_err(|error| {
            custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Could not decode to address from hex {error}"),
            )
        })?;
    let object_id = ObjectID::try_from(transfer_order_params.object_id)
        .map_err(|error| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{error}")))?;
    let gas_object_id = ObjectID::try_from(transfer_order_params.gas_object_id)
        .map_err(|error| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{error}")))?;
    let owner = decode_bytes_hex(transfer_order_params.from_address.as_str()).map_err(|error| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Could not decode address from hex {error}"),
        )
    })?;

    let mut wallet_context = server_context.wallet_context.lock().await;
    let wallet_context = wallet_context.as_mut().ok_or_else(|| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            "Wallet Context does not exist.".to_string(),
        )
    })?;

    let (cert, effects, gas_used) = match wallet_context
        .address_manager
        .transfer_object(owner, object_id, gas_object_id, to_address)
        .await
    {
        Ok((cert, effects)) => {
            let gas_used = match effects.status {
                ExecutionStatus::Success { gas_used } => gas_used,
                ExecutionStatus::Failure { gas_used, error } => {
                    return Err(custom_http_error(
                        StatusCode::FAILED_DEPENDENCY,
                        format!(
                            "Error trasnferring object: {:#?}, gas used {}",
                            error, gas_used
                        ),
                    ));
                }
            };
            (cert, effects, gas_used)
        }
        Err(err) => {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Transfer error: {err}"),
            ));
        }
    };

    let object_effects_summary = match get_object_effects(wallet_context, effects).await {
        Ok(effects) => effects,
        Err(err) => {
            return Err(err);
        }
    };

    custom_http_response(
        StatusCode::OK,
        TransactionResponse {
            gas_used,
            object_effects_summary: json!(object_effects_summary),
            certificate: json!(cert),
        },
    )
    .map_err(|err| custom_http_error(StatusCode::BAD_REQUEST, format!("{err}")))
}

/**
Request representing the contents of the Move module to be published.
*/
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PublishRequest {
    /** Required; Hex code as string representing the sender's address */
    sender: String,
    /** Required; Move module serialized as bytes? */
    module: String,
    /** Required; Hex code as string representing the gas object id */
    gas_object_id: String,
    /** Required; Gas budget required because of the need to execute module initializers */
    gas_budget: u64,
}

/**
Publish move module. It will perform proper verification and linking to make
sure the pacakge is valid. If some modules have initializers, these initializers
will also be executed in Move (which means new Move objects can be created in
the process of publishing a Move package). Gas budget is required because of the
need to execute module initializers.
 */
#[endpoint {
    method = POST,
    path = "/publish",
    tags = [ "wallet" ],
    // TODO: Figure out how to pass modules over the network before publishing this.
    unpublished = true
}]
#[allow(unused_variables)]
async fn publish(
    rqctx: Arc<RequestContext<ServerContext>>,
    request: TypedBody<PublishRequest>,
) -> Result<HttpResponseOk<TransactionResponse>, HttpError> {
    let transaction_response = TransactionResponse {
        gas_used: 0,
        object_effects_summary: json!(""),
        certificate: json!(""),
    };

    Ok(HttpResponseOk(transaction_response))
}

/**
Request containing the information required to execute a move module.
*/
// TODO: Adjust call specs based on how linter officially lands (pull#508)
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CallRequest {
    /** Required; Hex code as string representing the sender's address */
    sender: String,
    /** Required; Hex code as string representing Move module location */
    package_object_id: String,
    /** Required; Name of the move module */
    module: String,
    /** Required; Name of the function to be called in the move module */
    function: String,
    /** Optional; The argument types to be parsed */
    type_args: Option<Vec<String>>,
    /** Required; JSON representation of the arguments */
    args: Vec<SuiJsonValue>,
    /** Required; Hex code as string representing the gas object id */
    gas_object_id: String,
    /** Required; Gas budget required as a cap for gas usage */
    gas_budget: u64,
}

/**
Execute a Move call transaction by calling the specified function in the
module of the given package. Arguments are passed in and type will be
inferred from function signature. Gas usage is capped by the gas_budget.
Example CallRequest
{
    "sender": "b378b8d26c4daa95c5f6a2e2295e6e5f34371c1659e95f572788ffa55c265363",sss
    "package_object_id": "0x2",
    "module": "ObjectBasics",
    "function": "create",
    "args": [
        200,
        "b378b8d26c4daa95c5f6a2e2295e6e5f34371c1659e95f572788ffa55c265363"
    ],
    "gas_object_id": "1AC945CA31E77991654C0A0FCA8B0FD9C469B5C6",
    "gas_budget": 2000
}
 */
#[endpoint {
    method = POST,
    path = "/call",
    tags = [ "wallet" ],
}]
#[allow(unused_variables)]
async fn call(
    rqctx: Arc<RequestContext<ServerContext>>,
    request: TypedBody<CallRequest>,
) -> Result<Response<Body>, HttpError> {
    let server_context = rqctx.context();
    let call_params = request.into_inner();

    let module = Identifier::from_str(&call_params.module.to_owned()).map_err(|error| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Could not parse module name: {:?}", error),
        )
    })?;
    let function = Identifier::from_str(&call_params.function.to_owned()).map_err(|error| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Could not parse function name: {:?}", error),
        )
    })?;
    let args = call_params.args;
    // TODO: Figure out the fancier way to do this with iter/map/collect but also handle the error.
    let type_arg_strings: Vec<String> = match call_params.type_args {
        Some(args) => args,
        None => {
            let empty_vec: Vec<String> = vec![];
            empty_vec
        }
    };
    let mut type_args = vec![];
    for type_arg in type_arg_strings {
        type_args.push(parse_type_tag(&type_arg).map_err(|error| {
            custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Could not parse arg type: {:?}", error),
            )
        })?);
    }
    let gas_budget = call_params.gas_budget;

    let gas_object_id = ObjectID::try_from(call_params.gas_object_id)
        .map_err(|error| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{error}")))?;
    let package_object_id = ObjectID::from_hex_literal(&call_params.package_object_id)
        .map_err(|error| custom_http_error(StatusCode::FAILED_DEPENDENCY, format!("{error}")))?;

    let mut wallet_context_lock = server_context.wallet_context.lock().await;
    let wallet_context = wallet_context_lock.as_mut().ok_or_else(|| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            "Wallet Context does not exist.".to_string(),
        )
    })?;

    let sender: SuiAddress = match decode_bytes_hex(call_params.sender.as_str()) {
        Ok(sender) => sender,
        Err(error) => {
            return Err(HttpError::for_client_error(
                None,
                StatusCode::FAILED_DEPENDENCY,
                format!("Could not decode address from hex {error}"),
            ));
        }
    };

    let (package_object_ref, package_object, layout) =
        match get_object_info(wallet_context, package_object_id).await {
            Ok((object_ref, object, layout)) => (object_ref, object, layout),
            Err(error) => {
                return Err(error);
            }
        };

    // These steps can potentially be condensed and moved into the client/manager level
    // Extract the input args
    let (object_ids, pure_args) =
        match resolve_move_function_args(&package_object, module.clone(), function.clone(), args) {
            Ok(r) => r,
            Err(err) => {
                return Err(HttpError::for_client_error(
                    None,
                    StatusCode::FAILED_DEPENDENCY,
                    format!("Move call error {}.", err),
                ));
            }
        };

    info!("Resolved fn to: \n {:?} & {:?}", object_ids, pure_args);

    // Fetch all the objects needed for this call
    let mut input_objs = vec![];
    for obj_id in object_ids.clone() {
        input_objs.push(match get_object_info(wallet_context, obj_id).await {
            Ok((_, object, _)) => object,
            Err(error) => {
                return Err(error);
            }
        });
    }

    // Pass in the objects for a deeper check
    // We can technically move this to impl MovePackage
    if let Err(error) = resolve_and_type_check(
        package_object.clone(),
        &module,
        &function,
        &type_args,
        input_objs,
        pure_args.clone(),
    ) {
        return Err(custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Error while resolving and type checking: {:?}", error),
        ));
    };

    // Fetch the object info for the gas obj
    let gas_obj_ref = match get_object_info(wallet_context, gas_object_id).await {
        Ok((obj_ref, _, _)) => obj_ref,
        Err(error) => {
            return Err(error);
        }
    };

    // Fetch the objects for the object args
    let mut object_args_refs = Vec::new();
    for obj_id in object_ids {
        object_args_refs.push(match get_object_info(wallet_context, obj_id).await {
            Ok((obj_ref, _, _)) => obj_ref,
            Err(error) => {
                return Err(error);
            }
        });
    }

    let (cert, effects, gas_used) = match wallet_context
        .address_manager
        .move_call(
            sender,
            package_object_ref,
            module.to_owned(),
            function.to_owned(),
            type_args.clone(),
            gas_obj_ref,
            object_args_refs,
            vec![],
            pure_args,
            gas_budget,
        )
        .await
    {
        Ok((cert, effects)) => {
            let gas_used = match effects.status {
                ExecutionStatus::Success { gas_used } => gas_used,
                ExecutionStatus::Failure { gas_used, error } => {
                    println!("Error calling move function: {:#?}, gas used {}",
                    error, gas_used);
                    return Err(custom_http_error(
                        StatusCode::FAILED_DEPENDENCY,
                        format!(
                            "Error calling move function: {:#?}, gas used {}",
                            error, gas_used
                        ),
                    ));
                }
            };
            (cert, effects, gas_used)
        }
        Err(err) => {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Move call error: {err}"),
            ));
        }
    };

    let object_effects_summary = match get_object_effects(wallet_context, effects).await {
        Ok(effects) => effects,
        Err(err) => {
            return Err(err);
        }
    };

    custom_http_response(
        StatusCode::OK,
        TransactionResponse {
            gas_used,
            object_effects_summary: json!(object_effects_summary),
            certificate: json!(cert),
        },
    )
    .map_err(|err| custom_http_error(StatusCode::BAD_REQUEST, format!("{err}")))
}

/**
Request containing the address that requires a sync.
*/
// TODO: This call may not be required. Sync should not need to be triggered by user.
#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SyncRequest {
    /** Required; Hex code as string representing the address */
    address: String,
}

/**
Synchronize client state with authorities. This will fetch the latest information
on all objects owned by each address that is managed by this client state.
 */
#[endpoint {
    method = POST,
    path = "/sync",
    tags = [ "wallet" ],
}]
async fn sync(
    rqctx: Arc<RequestContext<ServerContext>>,
    request: TypedBody<SyncRequest>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let server_context = rqctx.context();
    let sync_params = request.into_inner();
    let address = decode_bytes_hex(sync_params.address.as_str()).map_err(|error| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Could not decode to address from hex {error}"),
        )
    })?;

    let mut wallet_context = server_context.wallet_context.lock().await;
    let wallet_context = wallet_context.as_mut().ok_or_else(|| {
        custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            "Wallet Context does not exist.".to_string(),
        )
    })?;

    // Attempt to create a new account, but continue if it already exists.
    if let Err(error) = wallet_context.create_account_state(&address) {
        info!("{:?}", error);
    }

    if let Err(err) = wallet_context
        .address_manager
        .sync_client_state(address)
        .await
    {
        return Err(custom_http_error(
            StatusCode::FAILED_DEPENDENCY,
            format!("Can't create client state: {err}"),
        ));
    }

    Ok(HttpResponseUpdatedNoContent())
}

async fn get_object_effects(
    wallet_context: &WalletContext,
    transaction_effects: TransactionEffects,
) -> Result<HashMap<String, Vec<HashMap<String, String>>>, HttpError> {
    let mut object_effects_summary = HashMap::new();
    if !transaction_effects.created.is_empty() {
        let mut effects = Vec::new();
        for ((object_id, sequence_number, object_digest), _) in transaction_effects.created {
            let effect = get_effect(wallet_context, object_id, sequence_number, object_digest)
                .await
                .map_err(|error| error)?;
            effects.push(effect);
        }
        object_effects_summary.insert(String::from("created_objects"), effects);
    }
    if !transaction_effects.mutated.is_empty() {
        let mut effects = Vec::new();
        for ((object_id, sequence_number, object_digest), _) in transaction_effects.mutated {
            let effect = get_effect(wallet_context, object_id, sequence_number, object_digest)
                .await
                .map_err(|error| error)?;
            effects.push(effect);
        }
        object_effects_summary.insert(String::from("mutated_objects"), effects);
    }
    if !transaction_effects.deleted.is_empty() {
        let mut effects = Vec::new();
        for (object_id, sequence_number, object_digest) in transaction_effects.deleted {
            let effect = get_effect(wallet_context, object_id, sequence_number, object_digest)
                .await
                .map_err(|error| error)?;
            effects.push(effect);
        }
        object_effects_summary.insert(String::from("deleted_objects"), effects);
    }
    Ok(object_effects_summary)
}

async fn get_effect(
    wallet_context: &WalletContext,
    object_id: ObjectID,
    sequence_number: SequenceNumber,
    object_digest: ObjectDigest,
) -> Result<HashMap<String, String>, HttpError> {
    let mut effect = HashMap::new();
    let object = match get_object_info(wallet_context, object_id).await {
        Ok((_, object, _)) => object,
        Err(error) => {
            return Err(error);
        }
    };
    effect.insert(
        "type".to_string(),
        object
            .data
            .type_()
            .map_or("Move Package".to_owned(), |type_| format!("{}", type_)),
    );
    effect.insert("id".to_string(), object_id.to_string());
    effect.insert("version".to_string(), format!("{:?}", sequence_number));
    effect.insert("object_digest".to_string(), format!("{:?}", object_digest));
    Ok(effect)
}

async fn get_object_info(
    wallet_context: &WalletContext,
    object_id: ObjectID,
) -> Result<(ObjectRef, SuiObject, Option<MoveStructLayout>), HttpError> {
    let (object_ref, object, layout) = match wallet_context
        .address_manager
        .get_object_info(object_id)
        .await
    {
        Ok(ObjectRead::Exists(object_ref, object, layout)) => (object_ref, object, layout),
        Ok(ObjectRead::Deleted(_)) => {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Object ({object_id}) was deleted."),
            ));
        }
        Ok(ObjectRead::NotExists(_)) => {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Object ({object_id}) does not exist."),
            ));
        }
        Err(error) => {
            return Err(custom_http_error(
                StatusCode::FAILED_DEPENDENCY,
                format!("Error while getting object info: {:?}", error),
            ));
        }
    };
    Ok((object_ref, object, layout))
}

fn custom_http_response<T: Serialize + JsonSchema>(
    status_code: StatusCode,
    response_body: T,
) -> Result<Response<Body>, anyhow::Error> {
    let body: Body = serde_json::to_string(&response_body)?.into();
    let res = Response::builder()
        .status(status_code)
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)?;
    Ok(res)
}

fn custom_http_error(status_code: http::StatusCode, message: String) -> HttpError {
    HttpError::for_client_error(None, status_code, message)
}
