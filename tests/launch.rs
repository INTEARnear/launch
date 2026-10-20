use std::collections::HashMap;

use intear_dex_types::{AssetId, DexId, Operation};
use near_jsonrpc_client::{JsonRpcClient, methods};
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_primitives::{
    action::{Action, DeployGlobalContractAction, GlobalContractDeployMode},
    transaction::{SignedTransaction, Transaction, TransactionV0},
    types::Finality,
    views::{FinalExecutionOutcomeViewEnum, FinalExecutionStatus, QueryRequest, TxExecutionStatus},
};
use near_sdk::{
    json_types::{Base64VecU8, U128},
    serde::Deserialize,
};
use near_workspaces::{
    Account, AccountId, Worker,
    network::Sandbox,
    result::ExecutionFinalResult,
    types::{AccessKey, Gas, KeyType, NearToken, SecretKey},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use xyk_dex_types::{CreatePoolArgs, FeeConfiguration, PoolType, V2FeeConfiguration};

const MAINNET_RPC_URL: &str = "https://rpc.mainnet.near.org";
const DEX: &str = "dex.intear.near";
const PLACH: &str = "slimedragon.near/xyk";
const TEAR: &str = "token.intear.near";
const FACTORY: &str = "launch-v2.intear.near";
const TOKEN_CODE_HASH: &str = "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF";
const STORAGE_BYTE_COST: u128 = 10u128.pow(19);
/// The token's storage_balance_bounds().min
const FT_STORAGE_DEPOSIT: NearToken = NearToken::from_yoctonear(125 * STORAGE_BYTE_COST);
const SHORT_ID_COST: NearToken = NearToken::from_near(1);
const LAUNCH_GAS: Gas = Gas::from_tgas(300);
/// The most that launch_token needs for each byte of its arguments, which is for escaped
/// characters in the name
const LAUNCH_GAS_PER_ARGS_BYTE: Gas = Gas::from_gas(400_000_000);
const QUOTE_TOKEN_LAUNCH_GAS: Gas = Gas::from_tgas(350);
const HOUR_NS: u64 = 60 * 60 * 1_000_000_000;

struct Wasms {
    factory: Vec<u8>,
    engine: Vec<u8>,
    plach: Vec<u8>,
    token: Vec<u8>,
}

static WASMS: OnceCell<Wasms> = OnceCell::const_new();

/// This contract built from the working tree, and the mainnet code of what it uses
async fn wasms() -> &'static Wasms {
    WASMS
        .get_or_init(|| async {
            let mainnet = near_workspaces::mainnet().await.unwrap();
            let dex: AccountId = DEX.parse().unwrap();
            let engine = mainnet.view_code(&dex).await.unwrap();
            let plach = mainnet
                .view(&dex, "dex_code")
                .args_json(json!({ "dex_id": PLACH }))
                .await
                .unwrap()
                .json::<Base64VecU8>()
                .unwrap()
                .0;
            let token = global_contract_code(TOKEN_CODE_HASH).await;
            let factory = near_workspaces::compile_project("./").await.unwrap();
            Wasms {
                factory,
                engine,
                plach,
                token,
            }
        })
        .await
}

async fn global_contract_code(code_hash: &str) -> Vec<u8> {
    let response = JsonRpcClient::connect(MAINNET_RPC_URL)
        .call(methods::query::RpcQueryRequest {
            block_reference: Finality::Final.into(),
            request: QueryRequest::ViewGlobalContractCode {
                code_hash: code_hash.parse().unwrap(),
            },
        })
        .await
        .unwrap();
    let QueryResponseKind::ViewCode(code) = response.kind else {
        panic!("Unexpected response to view_global_contract_code: {response:?}");
    };
    assert_eq!(
        near_sdk::bs58::encode(Sha256::digest(&code.code)).into_string(),
        code_hash
    );
    code.code
}

/// near-workspaces has no action for global contracts
async fn deploy_global_contract(worker: &Worker<Sandbox>, account: &Account, code: &[u8]) {
    let client = JsonRpcClient::connect(worker.rpc_addr());
    let signer_id: near_primitives::types::AccountId = account.id().as_str().parse().unwrap();
    let signer = near_crypto::InMemorySigner::from_secret_key(
        signer_id.clone(),
        account.secret_key().to_string().parse().unwrap(),
    );
    let access_key = client
        .call(methods::query::RpcQueryRequest {
            // The latest nonce, which may be in a block that isn't final yet
            block_reference: Finality::None.into(),
            request: QueryRequest::ViewAccessKey {
                account_id: signer_id.clone(),
                public_key: signer.public_key(),
            },
        })
        .await
        .unwrap();
    let QueryResponseKind::AccessKey(access_key) = access_key.kind else {
        panic!("Unexpected response to view_access_key: {access_key:?}");
    };
    let block = client
        .call(methods::block::RpcBlockRequest {
            block_reference: Finality::Final.into(),
        })
        .await
        .unwrap();
    let transaction = Transaction::V0(TransactionV0 {
        signer_id: signer_id.clone(),
        public_key: signer.public_key(),
        nonce: access_key.nonce + 1,
        receiver_id: signer_id,
        block_hash: block.header.hash,
        actions: vec![Action::DeployGlobalContract(DeployGlobalContractAction {
            code: code.into(),
            deploy_mode: GlobalContractDeployMode::CodeHash,
        })],
    });
    let (hash, _) = transaction.get_hash_and_size();
    let response = client
        .call(methods::send_tx::RpcSendTransactionRequest {
            signed_transaction: SignedTransaction::new(signer.sign(hash.as_ref()), transaction),
            wait_until: TxExecutionStatus::Final,
        })
        .await
        .unwrap();
    let Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(outcome)) =
        response.final_execution_outcome
    else {
        panic!("Unexpected response to send_tx");
    };
    assert!(
        matches!(outcome.status, FinalExecutionStatus::SuccessValue(_)),
        "{outcome:?}"
    );
}

struct Env {
    worker: Worker<Sandbox>,
    factory: Account,
    /// Has all TEAR
    alice: Account,
    bob: Account,
}

/// The accounts of a launch as on mainnet, including the one-time setup of the quote assets
/// that launch costs don't include
async fn setup() -> Env {
    let wasms = wasms().await;
    let worker = near_workspaces::sandbox().await.unwrap();
    let root = worker.root_account().unwrap();

    let near_key = SecretKey::from_random(KeyType::ED25519);
    worker
        .patch(&"near".parse().unwrap())
        .access_key(near_key.public_key(), AccessKey::full_access())
        .transact()
        .await
        .unwrap();
    let near = Account::from_secret_key("near".parse().unwrap(), near_key, &worker);
    let intear = create_account(&near, "intear", 10_000).await;
    let slimedragon = create_account(&near, "slimedragon", 1_000).await;
    let alice = create_account(&root, "alice", 10_000).await;
    let bob = create_account(&root, "bob", 1_000).await;

    deploy_global_contract(&worker, &root, &wasms.token).await;

    let dex = create_account(&intear, "dex", 100).await;
    dex.deploy(&wasms.engine)
        .await
        .unwrap()
        .into_result()
        .unwrap();
    call(
        &dex,
        DEX,
        "new",
        json!({ "trusted_code_deployer": slimedragon.id() }),
        NearToken::ZERO,
    )
    .await;
    call(
        &slimedragon,
        DEX,
        "dex_storage_deposit",
        json!({ "dex_id": PLACH }),
        NearToken::from_near(10),
    )
    .await;
    call(
        &slimedragon,
        DEX,
        "deploy_dex_code",
        json!({ "last_part_of_id": "xyk", "code_base64": Base64VecU8(wasms.plach.clone()) }),
        NearToken::from_yoctonear(1),
    )
    .await;
    call(
        &slimedragon,
        DEX,
        "dex_call",
        json!({ "dex_id": PLACH, "method": "new", "args": "", "attached_assets": {} }),
        NearToken::from_yoctonear(1),
    )
    .await;

    let tear = create_token(&intear, "token", alice.id(), "Intear", "TEAR").await;

    let factory = create_account(&intear, "launch-v2", 100).await;
    factory
        .deploy(&wasms.factory)
        .await
        .unwrap()
        .into_result()
        .unwrap();
    call(&factory, FACTORY, "new", json!({}), NearToken::ZERO).await;

    // NEAR and TEAR registered in Intear DEX for the factory, for Plach and in custody, and
    // dex.intear.near registered on TEAR
    for account_id in [DEX, FACTORY] {
        ft_storage_deposit(&root, tear.id().as_str(), account_id).await;
    }
    call(
        &factory,
        DEX,
        "storage_deposit",
        json!({}),
        NearToken::from_near(1),
    )
    .await;
    let quote_assets = [AssetId::Near, AssetId::Nep141(TEAR.parse().unwrap())];
    call(
        &factory,
        DEX,
        "register_assets",
        json!({ "asset_ids": quote_assets }),
        NearToken::from_yoctonear(1),
    )
    .await;
    call(
        &factory,
        DEX,
        "register_assets",
        json!({ "asset_ids": quote_assets, "for": { "Dex": PLACH } }),
        NearToken::from_yoctonear(1),
    )
    .await;

    // Plach pools quoted in NEAR and TEAR, which make plach.intear.near's fee records in them
    call(
        &intear,
        DEX,
        "storage_deposit",
        json!({}),
        NearToken::from_near(1),
    )
    .await;
    let mut operations = Vec::new();
    for (quote_asset, name) in quote_assets.into_iter().zip(["near-quoted", "tear-quoted"]) {
        let launched = create_token(&intear, name, intear.id(), "Warm-up", "WARM").await;
        let launched_asset = AssetId::Nep141(launched.id().as_str().parse().unwrap());
        ft_storage_deposit(&root, launched.id().as_str(), DEX).await;
        call(
            &intear,
            DEX,
            "register_assets",
            json!({ "asset_ids": [AssetId::Near, quote_asset.clone(), launched_asset.clone()] }),
            NearToken::from_yoctonear(1),
        )
        .await;
        call(
            &intear,
            DEX,
            "register_assets",
            json!({ "asset_ids": [quote_asset.clone(), launched_asset.clone()], "for": { "Dex": PLACH } }),
            NearToken::from_yoctonear(1),
        )
        .await;
        call(
            &intear,
            launched.id().as_str(),
            "ft_transfer_call",
            json!({ "receiver_id": DEX, "amount": U128(10u128.pow(30)), "msg": "" }),
            NearToken::from_yoctonear(1),
        )
        .await;
        operations.push(Operation::DexCall {
            dex_id: PLACH.parse::<DexId>().unwrap(),
            method: "create_pool".to_string(),
            args: Base64VecU8(
                near_sdk::borsh::to_vec(&CreatePoolArgs {
                    assets: (quote_asset, launched_asset.clone()),
                    fees: FeeConfiguration::V2(V2FeeConfiguration { receivers: vec![] }),
                    pool_type: PoolType::LaunchV2 {
                        phantom_liquidity: U128(10u128.pow(24)),
                    },
                    initial_liquidity: None,
                })
                .unwrap(),
            ),
            attached_assets: HashMap::from_iter([
                (
                    AssetId::Near,
                    U128(NearToken::from_millinear(100).as_yoctonear()),
                ),
                (launched_asset, U128(10u128.pow(30))),
            ]),
        });
    }
    call(
        &intear,
        DEX,
        "execute_operations",
        json!({ "operations": operations }),
        NearToken::from_near(1),
    )
    .await;

    Env {
        worker,
        factory,
        alice,
        bob,
    }
}

async fn create_account(parent: &Account, name: &str, near: u128) -> Account {
    parent
        .create_subaccount(name)
        .initial_balance(NearToken::from_near(near))
        .transact()
        .await
        .unwrap()
        .into_result()
        .unwrap()
}

async fn create_token(
    parent: &Account,
    name: &str,
    owner_id: &AccountId,
    token_name: &str,
    symbol: &str,
) -> Account {
    let token = create_account(parent, name, 10).await;
    token
        .deploy(&wasms().await.token)
        .await
        .unwrap()
        .into_result()
        .unwrap();
    call(
        &token,
        token.id().as_str(),
        "new",
        json!({
            "owner_id": owner_id,
            "total_supply": U128(10u128.pow(33)),
            "metadata": {
                "spec": "ft-1.0.0",
                "name": token_name,
                "symbol": symbol,
                "decimals": 24,
            },
        }),
        NearToken::ZERO,
    )
    .await;
    token
}

async fn ft_storage_deposit(payer: &Account, token_id: &str, account_id: &str) {
    call(
        payer,
        token_id,
        "storage_deposit",
        json!({ "account_id": account_id, "registration_only": true }),
        FT_STORAGE_DEPOSIT,
    )
    .await;
}

/// Asserts that the call and every receipt it made succeeded
async fn call(signer: &Account, receiver_id: &str, method: &str, args: Value, deposit: NearToken) {
    assert_succeeded(&try_call(signer, receiver_id, method, args, deposit).await);
}

async fn try_call(
    signer: &Account,
    receiver_id: &str,
    method: &str,
    args: Value,
    deposit: NearToken,
) -> ExecutionFinalResult {
    signer
        .call(&receiver_id.parse().unwrap(), method)
        .args_json(args)
        .deposit(deposit)
        .max_gas()
        .transact()
        .await
        .unwrap()
}

fn assert_succeeded(result: &ExecutionFinalResult) {
    assert!(
        result.is_success() && result.receipt_failures().is_empty(),
        "{result:#?}"
    );
}

/// The error of a launch or call that failed in its first receipt, before reaching a token or
/// Intear DEX
fn failure_of(result: ExecutionFinalResult) -> String {
    let factory: AccountId = FACTORY.parse().unwrap();
    assert!(
        result
            .receipt_outcomes()
            .iter()
            .all(|outcome| outcome.executor_id.as_str() != DEX
                && !outcome.executor_id.is_sub_account_of(&factory)),
        "{result:#?}"
    );
    format!("{:?}", result.into_result().unwrap_err())
}

async fn view<T: near_sdk::serde::de::DeserializeOwned>(
    env: &Env,
    contract_id: &str,
    method: &str,
    args: Value,
) -> T {
    env.worker
        .view(&contract_id.parse().unwrap(), method)
        .args_json(args)
        .await
        .unwrap()
        .json()
        .unwrap()
}

/// The token account that prepare_launch shows for launch_args(symbol)
async fn preview_id(env: &Env, symbol: &str, short_id: bool) -> AccountId {
    let mut args = launch_args(symbol);
    args["short_id"] = json!(short_id);
    let preparation = prepare_launch(env, &env.alice, &args).await;
    assert!(preparation.errors.is_empty(), "{preparation:?}");
    preparation.token_account_id.unwrap()
}

/// launch_token's arguments for a NEAR launch with a long ID, no fees and no first buy
fn launch_args(symbol: &str) -> Value {
    json!({
        "name": format!("{symbol} Token"),
        "symbol": symbol,
        "icon": "data:image/webp;base64,UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==",
        "decimals": 24,
        "total_supply": U128(10u128.pow(33)),
        "short_id": false,
        "fees": null,
        "launch_data": {
            "telegram": "https://t.me/intearchat",
            "x": null,
            "twitch": null,
            "website": "https://intea.rs",
            "description": "A test token",
        },
        "first_buy": null,
        "quote_token": null,
    })
}

fn u128_arg(value: &Value) -> u128 {
    value.as_str().unwrap().parse().unwrap()
}

#[derive(Deserialize, Debug)]
#[serde(crate = "near_sdk::serde")]
struct LaunchPreparation {
    token_account_id: Option<AccountId>,
    deposit: Option<NearToken>,
    errors: Vec<String>,
}

async fn prepare_launch(env: &Env, account: &Account, args: &Value) -> LaunchPreparation {
    let mut args = args.clone();
    args["account_id"] = json!(account.id());
    view(env, FACTORY, "prepare_launch", args).await
}

/// The deposit of a launch that prepare_launch finds no errors in
async fn launch_deposit(env: &Env, account: &Account, args: &Value) -> NearToken {
    let preparation = prepare_launch(env, account, args).await;
    assert!(preparation.errors.is_empty(), "{preparation:?}");
    preparation.deposit.unwrap()
}

async fn launch(
    env: &Env,
    account: &Account,
    args: &Value,
    deposit: NearToken,
    gas: Gas,
) -> ExecutionFinalResult {
    account
        .call(env.factory.id(), "launch_token")
        .args_json(args)
        .deposit(deposit)
        .gas(gas)
        .transact()
        .await
        .unwrap()
}

fn factory_receipts(result: &ExecutionFinalResult) -> usize {
    result
        .receipt_outcomes()
        .iter()
        .filter(|outcome| outcome.executor_id.as_str() == FACTORY)
        .count()
}

/// What launches make accounts store
struct StorageSnapshot {
    factory_bytes: u64,
    /// Used storage balances in Intear DEX
    factory_dex_used: u128,
    plach_dex_used: u128,
}

async fn storage_snapshot(env: &Env) -> StorageSnapshot {
    let used = |balance: Value| u128_arg(&balance["total"]) - u128_arg(&balance["available"]);
    StorageSnapshot {
        factory_bytes: env
            .worker
            .view_account(env.factory.id())
            .await
            .unwrap()
            .storage_usage,
        factory_dex_used: used(
            view(
                env,
                DEX,
                "storage_balance_of",
                json!({ "account_id": FACTORY }),
            )
            .await,
        ),
        plach_dex_used: used(
            view(
                env,
                DEX,
                "dex_storage_balance_of",
                json!({ "dex_id": PLACH }),
            )
            .await,
        ),
    }
}

/// Launches with what prepare_launch returns, and checks every receipt, the token, the pool, and
/// that the deposit is exactly what the launch used: the storage each account gained, the 2 token
/// registrations, the 1 yoctoNEAR attachments, the short ID cost and a NEAR first buy
async fn launch_exactly(env: &Env, account: &Account, args: &Value, gas: Gas) -> AccountId {
    let preparation = prepare_launch(env, account, args).await;
    assert!(preparation.errors.is_empty(), "{preparation:?}");
    let deposit = preparation.deposit.unwrap();
    let first_buy = (!args["first_buy"].is_null()).then(|| u128_arg(&args["first_buy"]));
    let quote_token = args["quote_token"].as_str();
    let quote_balance = match quote_token {
        Some(quote_token) => Some(
            view::<U128>(
                env,
                FACTORY,
                "get_quote_token_balance",
                json!({ "account_id": account.id(), "quote_token": quote_token }),
            )
            .await
            .0,
        ),
        None => None,
    };
    let before = storage_snapshot(env).await;
    let result = launch(env, account, args, deposit, gas).await;
    assert_succeeded(&result);
    // Plach would send unused NEAR back to the factory
    assert_eq!(factory_receipts(&result), 1, "{result:#?}");
    let token_id: AccountId = result.json().unwrap();
    assert_eq!(preparation.token_account_id.as_ref(), Some(&token_id));
    let after = storage_snapshot(env).await;

    // A balance record prefix ++ borsh(account_id) -> u128 for dex.intear.near and the creator
    let registration_bytes = |account_id: &str| 40 + 1 + 4 + account_id.len() as u64 + 16;
    let token_bytes = env
        .worker
        .view_account(&token_id)
        .await
        .unwrap()
        .storage_usage
        - registration_bytes(DEX)
        - registration_bytes(account.id().as_str());
    // A first buy with all deposited quote tokens removes their balance record, prefix ++
    // borsh((account_id, quote_token)) -> U128, which the storage deposit paid for
    let freed_quote_balance_bytes = match (quote_token, quote_balance, first_buy) {
        (Some(quote_token), Some(balance), Some(first_buy)) if balance == first_buy => {
            40 + 1 + (4 + account.id().len() as u64) + (4 + quote_token.len() as u64) + 16
        }
        _ => 0,
    };
    let expected_deposit = STORAGE_BYTE_COST
        * u128::from(
            after.factory_bytes + freed_quote_balance_bytes - before.factory_bytes + token_bytes,
        )
        + (after.factory_dex_used - before.factory_dex_used)
        + (after.plach_dex_used - before.plach_dex_used)
        + 2 * FT_STORAGE_DEPOSIT.as_yoctonear()
        + if quote_token.is_some() && first_buy.is_some() {
            4
        } else {
            3
        }
        + if args["short_id"].as_bool().unwrap() {
            SHORT_ID_COST.as_yoctonear()
        } else {
            0
        }
        + if quote_token.is_none() {
            first_buy.unwrap_or_default()
        } else {
            0
        };
    assert_eq!(deposit.as_yoctonear(), expected_deposit);

    let metadata: Value = view(env, token_id.as_str(), "ft_metadata", json!({})).await;
    for field in ["name", "symbol", "icon", "decimals"] {
        assert_eq!(metadata[field], args[field]);
    }
    let in_pool: U128 = view(
        env,
        DEX,
        "asset_balance_of",
        json!({ "of": { "Dex": PLACH }, "asset_id": AssetId::Nep141(token_id.as_str().parse().unwrap()) }),
    )
    .await;
    let bought: U128 = view(
        env,
        token_id.as_str(),
        "ft_balance_of",
        json!({ "account_id": account.id() }),
    )
    .await;
    assert_eq!(in_pool.0 + bought.0, u128_arg(&args["total_supply"]));
    assert_eq!(bought.0 > 0, first_buy.is_some());

    let launch_info: Value = view(
        env,
        FACTORY,
        "get_launch_data",
        json!({ "token_account_id": token_id }),
    )
    .await;
    assert_eq!(launch_info["launched_by"], json!(account.id()));
    for field in ["telegram", "x", "twitch", "website", "description"] {
        assert_eq!(launch_info[field], args["launch_data"][field]);
    }
    token_id
}

#[tokio::test]
async fn launches_with_near() {
    let env = setup().await;
    let args = launch_args("NEAR1");
    let preview = preview_id(&env, "NEAR1", false).await;
    assert_eq!(preview.as_str(), "near1-1.launch-v2.intear.near");
    assert_eq!(
        launch_exactly(&env, &env.alice, &args, LAUNCH_GAS).await,
        preview
    );

    // The symbol's counter exists now, so this launch stores less
    let preview = preview_id(&env, "NEAR1", false).await;
    assert_eq!(preview.as_str(), "near1-2.launch-v2.intear.near");
    assert_eq!(
        launch_exactly(&env, &env.bob, &args, LAUNCH_GAS).await,
        preview
    );
}

#[tokio::test]
async fn takes_only_the_exact_deposit() {
    let env = setup().await;
    let args = launch_args("EXACT");
    let deposit = launch_deposit(&env, &env.alice, &args).await;
    for wrong_deposit in [
        deposit.saturating_sub(NearToken::from_yoctonear(1)),
        deposit.saturating_add(NearToken::from_yoctonear(1)),
    ] {
        let error = failure_of(launch(&env, &env.alice, &args, wrong_deposit, LAUNCH_GAS).await);
        assert!(error.contains("Attach exactly"), "{error}");
    }
    assert_eq!(
        preview_id(&env, "EXACT", false).await.as_str(),
        "exact-1.launch-v2.intear.near"
    );
    launch_exactly(&env, &env.alice, &args, LAUNCH_GAS).await;
}

#[tokio::test]
async fn launches_with_short_id() {
    let env = setup().await;
    let mut args = launch_args("SHORT");
    args["short_id"] = json!(true);
    let token_id = launch_exactly(&env, &env.alice, &args, LAUNCH_GAS).await;
    assert_eq!(token_id.as_str(), "short.launch-v2.intear.near");
    let fees_earned: NearToken = view(&env, FACTORY, "fees_earned", json!({})).await;
    assert_eq!(fees_earned, SHORT_ID_COST);

    // Symbols are lowercased, so another case gives the same ID
    for symbol in ["SHORT", "short"] {
        args["symbol"] = json!(symbol);
        let preparation = prepare_launch(&env, &env.bob, &args).await;
        assert_eq!(
            preparation.errors,
            ["short.launch-v2.intear.near is already taken."]
        );
        assert_eq!(preparation.token_account_id, None);
        assert_eq!(preparation.deposit, None);
    }
}

#[tokio::test]
async fn launches_with_near_first_buy() {
    let env = setup().await;
    let mut args = launch_args("BUY");
    args["first_buy"] = json!(U128(NearToken::from_near(5).as_yoctonear()));
    launch_exactly(&env, &env.alice, &args, LAUNCH_GAS).await;
}

#[tokio::test]
async fn launches_with_quote_token_first_buy() {
    let env = setup().await;
    let mut args = launch_args("TEARBUY");
    args["quote_token"] = json!(TEAR);
    args["first_buy"] = json!(U128(10u128.pow(27)));
    assert_eq!(
        prepare_launch(&env, &env.alice, &args).await.errors,
        ["Deposit token.intear.near with ft_transfer_call for the first buy first."]
    );

    let bounds: Value = view(&env, FACTORY, "storage_balance_bounds", json!({})).await;
    call(
        &env.alice,
        FACTORY,
        "storage_deposit",
        json!({}),
        NearToken::from_yoctonear(u128_arg(&bounds["min"])),
    )
    .await;
    call(
        &env.alice,
        TEAR,
        "ft_transfer_call",
        json!({ "receiver_id": FACTORY, "amount": U128(2 * 10u128.pow(27)), "msg": "" }),
        NearToken::from_yoctonear(1),
    )
    .await;
    let mut too_large = args.clone();
    too_large["first_buy"] = json!(U128(3 * 10u128.pow(27)));
    assert_eq!(
        prepare_launch(&env, &env.alice, &too_large).await.errors,
        [
            "First buy is 3000000000000000000000000000 of token.intear.near, but only \
             2000000000000000000000000000 is deposited."
        ]
    );

    launch_exactly(&env, &env.alice, &args, QUOTE_TOKEN_LAUNCH_GAS).await;
    let balance: U128 = view(
        &env,
        FACTORY,
        "get_quote_token_balance",
        json!({ "account_id": env.alice.id(), "quote_token": TEAR }),
    )
    .await;
    assert_eq!(balance.0, 10u128.pow(27));
    // The rest, so the balance record is removed
    args["symbol"] = json!("TEARBUY2");
    launch_exactly(&env, &env.alice, &args, QUOTE_TOKEN_LAUNCH_GAS).await;
    let balance: U128 = view(
        &env,
        FACTORY,
        "get_quote_token_balance",
        json!({ "account_id": env.alice.id(), "quote_token": TEAR }),
    )
    .await;
    assert_eq!(balance.0, 0);
}

#[tokio::test]
async fn launches_with_many_fee_receivers() {
    let env = setup().await;
    let now = env.worker.view_block().await.unwrap().timestamp();
    // The longest account IDs, with the largest fee amounts
    let receivers: Vec<Value> = (0..19)
        .map(|i| {
            json!([
                { "Account": format!("{i:0>54}.test.near") },
                { "Scheduled": { "start": [now, 40_000], "end": [now + HOUR_NS, 500], "curve": "Linear" } },
            ])
        })
        .chain([json!(["Pool", { "Fixed": 1_000 }])])
        .collect();
    let mut args = launch_args("FEES");
    args["fees"] = json!(receivers);
    args["first_buy"] = json!(U128(NearToken::from_near(1).as_yoctonear()));
    launch_exactly(&env, &env.alice, &args, LAUNCH_GAS).await;

    // The receivers have fee records in NEAR now. The deposit still pays for them, since the
    // factory can't see them, and Plach sends what it doesn't use back to the factory.
    args["symbol"] = json!("FEES2");
    let deposit = launch_deposit(&env, &env.bob, &args).await;
    let result = launch(&env, &env.bob, &args, deposit, LAUNCH_GAS).await;
    assert_succeeded(&result);
    assert_eq!(factory_receipts(&result), 2, "{result:#?}");
}

#[tokio::test]
async fn rejects_launches_that_would_fail_later() {
    let env = setup().await;
    let now = env.worker.view_block().await.unwrap().timestamp();
    let with = |key: &str, value: Value| {
        let mut args = launch_args("BAD");
        args[key] = value;
        args
    };
    let account_fee = |account_id: &str, amount: Value| json!([{ "Account": account_id }, amount]);
    let mut short_hyphen = with("symbol", json!("A-B"));
    short_hyphen["short_id"] = json!(true);
    let cases = [
        (with("symbol", json!("A.B")), "Symbol cannot contain dots."),
        (
            short_hyphen,
            "Symbol cannot contain hyphens when using a short ID.",
        ),
        (
            with("total_supply", json!("0")),
            "Total supply must be positive.",
        ),
        (with("first_buy", json!("0")), "First buy must be positive."),
        (
            with("quote_token", json!("wrap.near")),
            "wrap.near can't be a quote token.",
        ),
        (
            with(
                "fees",
                json!([account_fee("bob.test.near", json!({ "Fixed": 900_001 }))]),
            ),
            "Invalid fees",
        ),
        (
            with(
                "fees",
                json!([account_fee(
                    "bob.test.near",
                    json!({ "Dynamic": { "min": 1, "max": 2 } })
                )]),
            ),
            "Invalid fees",
        ),
        (
            with(
                "fees",
                json!([account_fee(
                    "bob.test.near",
                    json!({ "Scheduled": { "start": [now, 1_000], "end": [now + HOUR_NS, 2_000], "curve": "Linear" } })
                )]),
            ),
            "Invalid fees",
        ),
        (
            with(
                "fees",
                json!(
                    (0..21)
                        .map(|i| account_fee(&format!("r{i}.test.near"), json!({ "Fixed": 1 })))
                        .collect::<Vec<_>>()
                ),
            ),
            "Invalid fees",
        ),
        (
            with(
                "fees",
                json!([account_fee("plach.intear.near", json!({ "Fixed": 1 }))]),
            ),
            "Invalid fees",
        ),
        (
            with(
                "fees",
                json!([["Pool", { "Fixed": 1 }], ["Pool", { "Fixed": 2 }]]),
            ),
            "Each fee receiver can be listed only once.",
        ),
        (
            with(
                "launch_data",
                json!({ "telegram": "https://telegram.me/intear" }),
            ),
            "Telegram URL must start with https://t.me/.",
        ),
        (
            with("launch_data", json!({ "x": "https://twitter.com/intear" })),
            "X URL must start with https://x.com/.",
        ),
        (
            with(
                "launch_data",
                json!({ "twitch": "https://www.twitch.tv/intear" }),
            ),
            "Twitch URL must start with https://twitch.tv/.",
        ),
        (
            with("launch_data", json!({ "website": "http://intea.rs" })),
            "Website URL must start with https://.",
        ),
    ];
    for (args, expected_error) in cases {
        let preparation = prepare_launch(&env, &env.alice, &args).await;
        assert!(
            preparation.errors.len() == 1 && preparation.errors[0].contains(expected_error),
            "{args}: {preparation:?}"
        );
        assert_eq!(preparation.deposit, None);
        // The symbol cases have no account to show
        assert_eq!(
            preparation.token_account_id.is_some(),
            args["symbol"] == "BAD",
            "{args}: {preparation:?}"
        );
        let error = failure_of(
            launch(
                &env,
                &env.alice,
                &args,
                NearToken::from_near(10),
                LAUNCH_GAS,
            )
            .await,
        );
        assert!(error.contains(expected_error), "{args}: {error}");
    }
    let mut args = launch_args("BAD");
    args["account_id"] = json!(DEX);
    assert_eq!(
        view::<LaunchPreparation>(&env, FACTORY, "prepare_launch", args)
            .await
            .errors,
        ["dex.intear.near can't launch tokens."]
    );

    // All of them at once
    let mut args = launch_args("A.B");
    args["total_supply"] = json!("0");
    args["quote_token"] = json!("wrap.near");
    args["first_buy"] = json!("1");
    args["fees"] = json!([["Pool", { "Fixed": 1 }], ["Pool", { "Fixed": 2 }]]);
    args["launch_data"] =
        json!({ "x": "https://twitter.com/intear", "website": "http://intea.rs" });
    let expected_errors = [
        "X URL must start with https://x.com/.",
        "Website URL must start with https://.",
        "Total supply must be positive.",
        "wrap.near can't be a quote token.",
        "Each fee receiver can be listed only once.",
        "Symbol cannot contain dots.",
    ];
    let preparation = prepare_launch(&env, &env.alice, &args).await;
    assert_eq!(preparation.errors, expected_errors);
    assert_eq!(preparation.token_account_id, None);
    assert_eq!(preparation.deposit, None);
    let error = failure_of(
        launch(
            &env,
            &env.alice,
            &args,
            NearToken::from_near(10),
            LAUNCH_GAS,
        )
        .await,
    );
    assert!(error.contains(&expected_errors.join(" ")), "{error}");

    assert_eq!(
        preview_id(&env, "BAD", false).await.as_str(),
        "bad-1.launch-v2.intear.near"
    );
}

#[tokio::test]
async fn needs_more_gas_for_large_name_and_icon() {
    let env = setup().await;
    let mut args = launch_args("BIG");
    // Escaped characters take the most gas per byte to parse
    args["name"] = json!("\"\\".repeat(5_000));
    args["icon"] = json!(format!("data:image/png;base64,{}", "A".repeat(300_000)));
    // Links and the description have no length limit
    args["launch_data"]["website"] = json!(format!("https://intea.rs/{}", "a".repeat(1_000)));
    args["launch_data"]["description"] = json!("A long description. ".repeat(5_000));
    let deposit = launch_deposit(&env, &env.alice, &args).await;
    let error = failure_of(launch(&env, &env.alice, &args, deposit, LAUNCH_GAS).await);
    assert!(error.contains("Exceeded the prepaid gas"), "{error}");
    assert_eq!(
        preview_id(&env, "BIG", false).await.as_str(),
        "big-1.launch-v2.intear.near"
    );
    let gas = LAUNCH_GAS
        .checked_add(
            LAUNCH_GAS_PER_ARGS_BYTE
                .checked_mul(args.to_string().len() as u64)
                .unwrap(),
        )
        .unwrap();
    launch_exactly(&env, &env.alice, &args, gas).await;
}

#[tokio::test]
async fn edits_tokens() {
    let env = setup().await;
    let token_id = launch_exactly(&env, &env.alice, &launch_args("EDIT"), LAUNCH_GAS).await;
    let edit = |account: &Account, description: &str, deposit: NearToken| {
        account
            .call(env.factory.id(), "edit_token")
            .args_json(json!({
                "token_account_id": token_id,
                "launch_data": { "description": description },
            }))
            .deposit(deposit)
            .max_gas()
            .transact()
    };
    let long = "A".repeat(1_000);
    let error = failure_of(edit(&env.alice, &long, NearToken::ZERO).await.unwrap());
    assert!(
        error.contains("Attach at least 1 yoctoNEAR to edit a token."),
        "{error}"
    );
    let error = failure_of(
        edit(&env.bob, &long, NearToken::from_near(1))
            .await
            .unwrap(),
    );
    assert!(
        error.contains("Only token creator can edit own tokens"),
        "{error}"
    );
    let error = failure_of(
        edit(&env.alice, &long, NearToken::from_yoctonear(1))
            .await
            .unwrap(),
    );
    assert!(
        error.contains("Insufficient deposit for storage cost."),
        "{error}"
    );

    // Takes the storage cost of the longer data and refunds the rest
    let bytes_before = env
        .worker
        .view_account(env.factory.id())
        .await
        .unwrap()
        .storage_usage;
    let result = edit(&env.alice, &long, NearToken::from_near(1))
        .await
        .unwrap();
    assert_succeeded(&result);
    assert!(result.logs().iter().any(|log| log.contains("token_edited")));
    let bytes_after = env
        .worker
        .view_account(env.factory.id())
        .await
        .unwrap()
        .storage_usage;
    assert!(bytes_after > bytes_before);
    let launch_info: Value = view(
        &env,
        FACTORY,
        "get_launch_data",
        json!({ "token_account_id": token_id }),
    )
    .await;
    assert_eq!(launch_info["description"], json!(long));

    // Refunds the storage that shorter data frees, with the 1 yoctoNEAR
    let result = edit(&env.alice, "Short", NearToken::from_yoctonear(1))
        .await
        .unwrap();
    assert_succeeded(&result);
    let bytes = env
        .worker
        .view_account(env.factory.id())
        .await
        .unwrap()
        .storage_usage;
    assert!(bytes < bytes_after);
    assert!(
        result
            .receipt_outcomes()
            .iter()
            .any(|outcome| outcome.executor_id == *env.alice.id())
    );
}

#[tokio::test]
async fn withdraws_fees() {
    let env = setup().await;
    let mut args = launch_args("FEE");
    args["short_id"] = json!(true);
    launch_exactly(&env, &env.alice, &args, LAUNCH_GAS).await;

    let error = failure_of(
        try_call(
            &env.alice,
            FACTORY,
            "withdraw_fees",
            json!({ "to": env.alice.id() }),
            NearToken::ZERO,
        )
        .await,
    );
    assert!(error.contains("is private"), "{error}");

    // A transfer to an account that doesn't exist fails, which keeps the fees
    let result = try_call(
        &env.factory,
        FACTORY,
        "withdraw_fees",
        json!({ "to": "missing.near" }),
        NearToken::ZERO,
    )
    .await;
    assert!(result.is_success());
    assert_eq!(result.receipt_failures().len(), 1, "{result:#?}");
    let fees_earned: NearToken = view(&env, FACTORY, "fees_earned", json!({})).await;
    assert_eq!(fees_earned, SHORT_ID_COST);

    let balance_before = env.worker.view_account(env.bob.id()).await.unwrap().balance;
    call(
        &env.factory,
        FACTORY,
        "withdraw_fees",
        json!({ "to": env.bob.id() }),
        NearToken::ZERO,
    )
    .await;
    let balance_after = env.worker.view_account(env.bob.id()).await.unwrap().balance;
    assert_eq!(balance_after, balance_before.saturating_add(SHORT_ID_COST));
    let fees_earned: NearToken = view(&env, FACTORY, "fees_earned", json!({})).await;
    assert_eq!(fees_earned, NearToken::ZERO);

    let error = failure_of(
        try_call(
            &env.factory,
            FACTORY,
            "withdraw_fees",
            json!({ "to": env.bob.id() }),
            NearToken::ZERO,
        )
        .await,
    );
    assert!(error.contains("No fees to withdraw."), "{error}");
}

#[tokio::test]
async fn holds_quote_token_deposits() {
    let env = setup().await;
    let amount = U128(10u128.pow(27));
    let tear_balance = |account: AccountId| {
        view::<U128>(
            &env,
            TEAR,
            "ft_balance_of",
            json!({ "account_id": account }),
        )
    };
    let quote_balance = |account: AccountId| {
        view::<U128>(
            &env,
            FACTORY,
            "get_quote_token_balance",
            json!({ "account_id": account, "quote_token": TEAR }),
        )
    };
    ft_storage_deposit(&env.bob, TEAR, env.bob.id().as_str()).await;
    call(
        &env.alice,
        TEAR,
        "ft_transfer",
        json!({ "receiver_id": env.bob.id(), "amount": amount }),
        NearToken::from_yoctonear(1),
    )
    .await;

    // Deposits from accounts that aren't registered are refunded
    let result = try_call(
        &env.bob,
        TEAR,
        "ft_transfer_call",
        json!({ "receiver_id": FACTORY, "amount": amount, "msg": "" }),
        NearToken::from_yoctonear(1),
    )
    .await;
    assert!(
        format!("{result:?}").contains("Register bob.test.near with storage_deposit"),
        "{result:#?}"
    );
    assert_eq!(tear_balance(env.bob.id().clone()).await, amount);

    let bounds: Value = view(&env, FACTORY, "storage_balance_bounds", json!({})).await;
    let min = u128_arg(&bounds["min"]);
    assert_eq!(bounds["max"], bounds["min"]);
    let error = failure_of(
        try_call(
            &env.bob,
            FACTORY,
            "storage_deposit",
            json!({}),
            NearToken::from_yoctonear(min - 1),
        )
        .await,
    );
    assert!(
        error.contains("Insufficient deposit for storage cost."),
        "{error}"
    );
    call(
        &env.bob,
        FACTORY,
        "storage_deposit",
        json!({}),
        NearToken::from_yoctonear(min),
    )
    .await;
    let storage_balance: Value = view(
        &env,
        FACTORY,
        "storage_balance_of",
        json!({ "account_id": env.bob.id() }),
    )
    .await;
    assert_eq!(u128_arg(&storage_balance["total"]), min);
    let error = failure_of(
        try_call(
            &env.bob,
            FACTORY,
            "storage_unregister",
            json!({}),
            NearToken::from_yoctonear(1),
        )
        .await,
    );
    assert!(error.contains("Unregistering is not supported."), "{error}");

    call(
        &env.bob,
        TEAR,
        "ft_transfer_call",
        json!({ "receiver_id": FACTORY, "amount": amount, "msg": "" }),
        NearToken::from_yoctonear(1),
    )
    .await;
    assert_eq!(quote_balance(env.bob.id().clone()).await, amount);
    assert_eq!(tear_balance(env.bob.id().clone()).await, U128(0));

    call(
        &env.bob,
        FACTORY,
        "withdraw_quote_token",
        json!({ "quote_token": TEAR }),
        NearToken::from_yoctonear(1),
    )
    .await;
    assert_eq!(quote_balance(env.bob.id().clone()).await, U128(0));
    assert_eq!(tear_balance(env.bob.id().clone()).await, amount);
}
