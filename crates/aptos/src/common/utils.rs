// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    common::types::{
        account_address_from_public_key, CliError, CliTypedResult, PromptOptions,
        TransactionOptions, TransactionSummary,
    },
    config::GlobalConfig,
    CliResult,
};
use aptos_build_info::build_information;
use aptos_crypto::ed25519::{Ed25519PrivateKey, Ed25519PublicKey};
use aptos_keygen::KeyGen;
use aptos_logger::{debug, Level};
use aptos_rest_client::{aptos_api_types::HashValue, Account, Client, State};
use aptos_telemetry::service::telemetry_is_disabled;
use aptos_types::{
    account_address::create_multisig_account_address,
    chain_id::ChainId,
    transaction::{authenticator::AuthenticationKey, TransactionPayload},
};
use itertools::Itertools;
use move_core_types::account_address::AccountAddress;
use reqwest::Url;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::BTreeMap,
    env,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant, SystemTime},
};
use tokio::time::timeout;

/// Prompts for confirmation until a yes or no is given explicitly
pub fn prompt_yes(prompt: &str) -> bool {
    let mut result: Result<bool, ()> = Err(());

    // Read input until a yes or a no is given
    while result.is_err() {
        println!("{} [yes/no] >", prompt);
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            continue;
        }
        result = match input.trim().to_lowercase().as_str() {
            "yes" | "y" => Ok(true),
            "no" | "n" => Ok(false),
            _ => Err(()),
        };
    }
    result.unwrap()
}

/// Convert any successful response to Success
pub async fn to_common_success_result<T>(
    command: &str,
    start_time: Instant,
    result: CliTypedResult<T>,
) -> CliResult {
    to_common_result(command, start_time, result.map(|_| "Success")).await
}

/// For pretty printing outputs in JSON
pub async fn to_common_result<T: Serialize>(
    command: &str,
    start_time: Instant,
    result: CliTypedResult<T>,
) -> CliResult {
    let latency = start_time.elapsed();
    let is_err = result.is_err();

    if !telemetry_is_disabled() {
        let error = if let Err(ref error) = result {
            // Only print the error type
            Some(error.to_str())
        } else {
            None
        };

        if let Err(err) = timeout(
            Duration::from_millis(2000),
            send_telemetry_event(command, latency, !is_err, error),
        )
        .await
        {
            debug!("send_telemetry_event timeout from CLI: {}", err.to_string())
        }
    }

    let result: ResultWrapper<T> = result.into();
    let string = serde_json::to_string_pretty(&result).unwrap();
    if is_err {
        Err(string)
    } else {
        Ok(string)
    }
}

pub fn cli_build_information() -> BTreeMap<String, String> {
    build_information!()
}

/// Sends a telemetry event about the CLI build, command and result
async fn send_telemetry_event(
    command: &str,
    latency: Duration,
    success: bool,
    error: Option<&str>,
) {
    // Collect the build information
    let build_information = cli_build_information();

    // Send the event
    aptos_telemetry::cli_metrics::send_cli_telemetry_event(
        build_information,
        command.into(),
        latency,
        success,
        error,
    )
    .await;
}

/// A result wrapper for displaying either a correct execution result or an error.
///
/// The purpose of this is to have a pretty easy to recognize JSON output format e.g.
///
/// {
///   "Result":{
///     "encoded":{ ... }
///   }
/// }
///
/// {
///   "Error":"Failed to run command"
/// }
///
#[derive(Debug, Serialize)]
enum ResultWrapper<T> {
    Result(T),
    Error(String),
}

impl<T> From<CliTypedResult<T>> for ResultWrapper<T> {
    fn from(result: CliTypedResult<T>) -> Self {
        match result {
            Ok(inner) => ResultWrapper::Result(inner),
            Err(inner) => ResultWrapper::Error(inner.to_string()),
        }
    }
}

/// Checks if a file exists, being overridden by `PromptOptions`
pub fn check_if_file_exists(file: &Path, prompt_options: PromptOptions) -> CliTypedResult<()> {
    if file.exists() {
        prompt_yes_with_override(
            &format!(
                "{:?} already exists, are you sure you want to overwrite it?",
                file.as_os_str(),
            ),
            prompt_options,
        )?
    }

    Ok(())
}

pub fn prompt_yes_with_override(prompt: &str, prompt_options: PromptOptions) -> CliTypedResult<()> {
    if prompt_options.assume_no {
        return Err(CliError::AbortedError);
    } else if prompt_options.assume_yes {
        return Ok(());
    }

    let is_yes = if let Some(response) = GlobalConfig::load()?.get_default_prompt_response() {
        response
    } else {
        prompt_yes(prompt)
    };

    if is_yes {
        Ok(())
    } else {
        Err(CliError::AbortedError)
    }
}

pub fn read_from_file(path: &Path) -> CliTypedResult<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| CliError::UnableToReadFile(format!("{}", path.display()), e.to_string()))
}

/// Write a `&[u8]` to a file
pub fn write_to_file(path: &Path, name: &str, bytes: &[u8]) -> CliTypedResult<()> {
    write_to_file_with_opts(path, name, bytes, &mut OpenOptions::new())
}

/// Write a User only read / write file
pub fn write_to_user_only_file(path: &Path, name: &str, bytes: &[u8]) -> CliTypedResult<()> {
    let mut opts = OpenOptions::new();
    #[cfg(unix)]
    opts.mode(0o600);
    write_to_file_with_opts(path, name, bytes, &mut opts)
}

/// Write a `&[u8]` to a file with the given options
pub fn write_to_file_with_opts(
    path: &Path,
    name: &str,
    bytes: &[u8],
    opts: &mut OpenOptions,
) -> CliTypedResult<()> {
    let mut file = opts
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| CliError::IO(name.to_string(), e))?;
    file.write_all(bytes)
        .map_err(|e| CliError::IO(name.to_string(), e))
}

/// Appends a file extension to a `Path` without overwriting the original extension.
pub fn append_file_extension(
    file: &Path,
    appended_extension: &'static str,
) -> CliTypedResult<PathBuf> {
    let extension = file
        .extension()
        .map(|extension| extension.to_str().unwrap_or_default());
    if let Some(extension) = extension {
        Ok(file.with_extension(extension.to_owned() + "." + appended_extension))
    } else {
        Ok(file.with_extension(appended_extension))
    }
}

/// Retrieves account resource from the rest client
pub async fn get_account(
    client: &aptos_rest_client::Client,
    address: AccountAddress,
) -> CliTypedResult<Account> {
    let account_response = client
        .get_account(address)
        .await
        .map_err(|err| CliError::ApiError(err.to_string()))?;
    Ok(account_response.into_inner())
}

/// Retrieves account resource from the rest client
pub async fn get_account_with_state(
    client: &aptos_rest_client::Client,
    address: AccountAddress,
) -> CliTypedResult<(Account, State)> {
    let account_response = client
        .get_account(address)
        .await
        .map_err(|err| CliError::ApiError(err.to_string()))?;
    Ok(account_response.into_parts())
}

/// Retrieves sequence number from the rest client
pub async fn get_sequence_number(
    client: &aptos_rest_client::Client,
    address: AccountAddress,
) -> CliTypedResult<u64> {
    Ok(get_account(client, address).await?.sequence_number)
}

/// Retrieves the auth key from the rest client
pub async fn get_auth_key(
    client: &aptos_rest_client::Client,
    address: AccountAddress,
) -> CliTypedResult<AuthenticationKey> {
    Ok(get_account(client, address).await?.authentication_key)
}

/// Retrieves the chain id from the rest client
pub async fn chain_id(rest_client: &Client) -> CliTypedResult<ChainId> {
    let state = rest_client
        .get_ledger_information()
        .await
        .map_err(|err| CliError::ApiError(err.to_string()))?
        .into_inner();
    Ok(ChainId::new(state.chain_id))
}
/// Error message for parsing a map
const PARSE_MAP_SYNTAX_MSG: &str = "Invalid syntax for map. Example: Name=Value,Name2=Value";

/// Parses an inline map of values
///
/// Example: Name=Value,Name2=Value
pub fn parse_map<K: FromStr + Ord, V: FromStr>(str: &str) -> anyhow::Result<BTreeMap<K, V>>
where
    K::Err: 'static + std::error::Error + Send + Sync,
    V::Err: 'static + std::error::Error + Send + Sync,
{
    let mut map = BTreeMap::new();

    // Split pairs by commas
    for pair in str.split_terminator(',') {
        // Split pairs by = then trim off any spacing
        let (first, second): (&str, &str) = pair
            .split_terminator('=')
            .collect_tuple()
            .ok_or_else(|| anyhow::Error::msg(PARSE_MAP_SYNTAX_MSG))?;
        let first = first.trim();
        let second = second.trim();
        if first.is_empty() || second.is_empty() {
            return Err(anyhow::Error::msg(PARSE_MAP_SYNTAX_MSG));
        }

        // At this point, we just give error messages appropriate to parsing
        let key: K = K::from_str(first)?;
        let value: V = V::from_str(second)?;
        map.insert(key, value);
    }
    Ok(map)
}

/// Generate a vanity account for Ed25519 single signer scheme, either standard or multisig.
///
/// The default authentication key for an Ed25519 account is the same as the account address. Hence
/// for a standard account, this function generates Ed25519 private keys until finding one that has
/// an authentication key (account address) that begins with the given vanity prefix.
///
/// For a multisig account, this function generates private keys until finding one that can create
/// a multisig account with the given vanity prefix as its first transaction (sequence number 0).
///
/// Note that while a valid hex string must have an even number of characters, a vanity prefix can
/// have an odd number of characters since account addresses are human-readable.
///
/// `vanity_prefix_ref` is a reference to a hex string vanity prefix, optionally prefixed with "0x".
/// For example "0xaceface" or "d00d".
pub fn generate_vanity_account_ed25519(
    vanity_prefix_ref: &str,
    multisig: bool,
) -> CliTypedResult<Ed25519PrivateKey> {
    let vanity_prefix_ref = vanity_prefix_ref
        .strip_prefix("0x")
        .unwrap_or(vanity_prefix_ref); // Optionally strip leading 0x from input string.
    let mut to_check_if_is_hex = String::from(vanity_prefix_ref);
    // If an odd number of characters append a 0 for verifying that prefix contains valid hex.
    if to_check_if_is_hex.len() % 2 != 0 {
        to_check_if_is_hex += "0"
    };
    hex::decode(to_check_if_is_hex).  // Check that the vanity prefix can be decoded into hex.
        map_err(|error| CliError::CommandArgumentError(format!(
            "The vanity prefix could not be decoded to hex: {}", error)))?;
    let mut key_generator = KeyGen::from_os_rng(); // Get random key generator.
    loop {
        // Generate new keys until finding a match against the vanity prefix.
        let private_key = key_generator.generate_ed25519_private_key();
        let mut account_address =
            account_address_from_public_key(&Ed25519PublicKey::from(&private_key));
        if multisig {
            account_address = create_multisig_account_address(account_address, 0)
        };
        if account_address
            .short_str_lossless()
            .starts_with(vanity_prefix_ref)
        {
            return Ok(private_key);
        };
    }
}

pub fn current_dir() -> CliTypedResult<PathBuf> {
    env::current_dir().map_err(|err| {
        CliError::UnexpectedError(format!("Failed to get current directory {}", err))
    })
}

pub fn dir_default_to_current(maybe_dir: Option<PathBuf>) -> CliTypedResult<PathBuf> {
    if let Some(dir) = maybe_dir {
        Ok(dir)
    } else {
        current_dir()
    }
}

pub fn create_dir_if_not_exist(dir: &Path) -> CliTypedResult<()> {
    // Check if the directory exists, if it's not a dir, it will also fail here
    if !dir.exists() || !dir.is_dir() {
        std::fs::create_dir_all(dir).map_err(|e| CliError::IO(dir.display().to_string(), e))?;
        debug!("Created {} folder", dir.display());
    } else {
        debug!("{} folder already exists", dir.display());
    }
    Ok(())
}

/// Reads a line from input
pub fn read_line(input_name: &'static str) -> CliTypedResult<String> {
    let mut input_buf = String::new();
    let _ = std::io::stdin()
        .read_line(&mut input_buf)
        .map_err(|err| CliError::IO(input_name.to_string(), err))?;

    Ok(input_buf)
}

/// Fund account (and possibly create it) from a faucet
pub async fn fund_account(
    faucet_url: Url,
    num_octas: u64,
    address: AccountAddress,
) -> CliTypedResult<Vec<HashValue>> {
    let response = reqwest::Client::new()
        .post(format!(
            "{}mint?amount={}&auth_key={}",
            faucet_url, num_octas, address
        ))
        .body("{}")
        .send()
        .await
        .map_err(|err| {
            CliError::ApiError(format!("Failed to fund account with faucet: {:#}", err))
        })?;
    if response.status() == 200 {
        let hashes: Vec<HashValue> = response
            .json()
            .await
            .map_err(|err| CliError::UnexpectedError(err.to_string()))?;
        Ok(hashes)
    } else {
        Err(CliError::ApiError(format!(
            "Faucet issue: {}",
            response.status()
        )))
    }
}

/// Wait for transactions, returning an error if any of them fail.
pub async fn wait_for_transactions(
    client: &aptos_rest_client::Client,
    hashes: Vec<HashValue>,
) -> CliTypedResult<()> {
    let sys_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| CliError::UnexpectedError(e.to_string()))?
        .as_secs()
        + 30;
    for hash in hashes {
        client
            .wait_for_transaction_by_hash(
                hash.into(),
                sys_time,
                Some(Duration::from_secs(60)),
                None,
            )
            .await?;
    }
    Ok(())
}

pub fn start_logger(level: Level) {
    let mut logger = aptos_logger::Logger::new();
    logger.channel_size(1000).is_async(false).level(level);
    logger.build();
}

/// For transaction payload and options, either get gas profile or submit for execution.
pub async fn profile_or_submit(
    payload: TransactionPayload,
    txn_options_ref: &TransactionOptions,
) -> CliTypedResult<TransactionSummary> {
    // Profile gas if needed.
    if txn_options_ref.profile_gas {
        txn_options_ref.profile_gas(payload).await
    } else {
        // Otherwise submit the transaction.
        txn_options_ref
            .submit_transaction(payload)
            .await
            .map(TransactionSummary::from)
    }
}

/// Try parsing JSON in file at path into a specified type.
pub fn parse_json_file<T: for<'a> Deserialize<'a>>(path_ref: &Path) -> CliTypedResult<T> {
    serde_json::from_slice::<T>(&read_from_file(path_ref)?).map_err(|err| {
        CliError::UnableToReadFile(format!("{}", path_ref.display()), err.to_string())
    })
}

/// Convert a view function JSON field into a string option.
///
/// A view function JSON return represents an option via an inner JSON array titled `vec`.
pub fn view_json_option_str(option_ref: &serde_json::Value) -> CliTypedResult<Option<String>> {
    if let Some(vec_field) = option_ref.get("vec") {
        if let Some(vec_array) = vec_field.as_array() {
            if vec_array.is_empty() {
                Ok(None)
            } else if vec_array.len() > 1 {
                Err(CliError::UnexpectedError(format!(
                    "JSON `vec` array has more than one element: {:?}",
                    vec_array
                )))
            } else {
                let option_val_ref = &vec_array[0];
                if let Some(inner_str) = option_val_ref.as_str() {
                    Ok(Some(inner_str.to_string()))
                } else {
                    Err(CliError::UnexpectedError(format!(
                        "JSON option is not a string: {}",
                        option_val_ref
                    )))
                }
            }
        } else {
            Err(CliError::UnexpectedError(format!(
                "JSON `vec` field is not an array: {}",
                vec_field
            )))
        }
    } else {
        Err(CliError::UnexpectedError(format!(
            "JSON field does not have an inner `vec` field: {}",
            option_ref
        )))
    }
}
