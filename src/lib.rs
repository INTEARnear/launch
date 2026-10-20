use std::collections::HashMap;

use intear_dex_types::{
    AssetId, DexId, Operation, SwapOperationAmount, SwapRequestAmount, WithdrawAmount,
};
use near_contract_standards::{
    fungible_token::{Balance, metadata::FungibleTokenMetadata, receiver::FungibleTokenReceiver},
    storage_management::{StorageBalance, StorageBalanceBounds, StorageManagement},
};
use near_sdk::{
    AccountId, AccountIdRef, BorshStorageKey, Gas, IntoStorageKey, NearToken, PanicOnDefault,
    Promise, PromiseError, PromiseOrValue, Timestamp,
    borsh::BorshSerialize,
    json_types::{Base64VecU8, U128},
    near, require,
    store::{IterableMap, LookupMap},
};
use xyk_dex_types::{
    CreatePoolArgs, FULL_SPLIT_FRACTION, FeeAmount, FeeConfiguration, FeeReceiver,
    PROTOCOL_FEE_RECEIVER_ID, PoolType, SwapArgs, SwapRoute, V2FeeConfiguration,
};

const SHORT_ID_COST: NearToken = NearToken::from_near(1);
/// Intear DEX rejects smaller storage deposits
const INTEAR_DEX_MIN_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(5);
const TOKEN_INIT_GAS: Gas = Gas::from_tgas(35);
const TOKEN_INIT_GAS_PER_ARGS_BYTE: Gas = Gas::from_gas(120_000_000);

// https://github.com/INTEARnear/global-ft-contract
const TOKEN_CODE_HASH: &str = "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF";
const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
// Launch costs don't include one-time setup of quote assets, so NEAR and each quote token must
// already be registered in Intear DEX for this contract, for Plach and in custody, Plach must
// already have a plach.intear.near fee entry for it (any Plach pool with it creates one), and
// dex.intear.near must be registered on each quote token.
const USDC_TOKEN_ID: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";
const TEAR_TOKEN_ID: &str = "token.intear.near";
const QUOTE_TOKEN_IDS: [&str; 2] = [USDC_TOKEN_ID, TEAR_TOKEN_ID];

const STORAGE_ENTRY_COST_BYTES: u128 = 40;
const STRING_PREFIX_BORSH_BYTES: u128 = 4;

fn quote_token_storage_deposit() -> NearToken {
    let account_id_bytes = STRING_PREFIX_BORSH_BYTES + AccountId::MAX_LEN as u128;
    let token_storage_bytes = STORAGE_ENTRY_COST_BYTES
        + StorageKey::StorageDeposits.into_storage_key().len() as u128
        + account_id_bytes
        + NearToken::ZERO.as_yoctonear().to_be_bytes().len() as u128;
    let one_quote_token_storage_bytes = STORAGE_ENTRY_COST_BYTES
        + StorageKey::QuoteTokenBalances.into_storage_key().len() as u128
        + account_id_bytes
        + account_id_bytes
        + (0 as Balance).to_be_bytes().len() as u128;
    near_sdk::env::storage_byte_cost().saturating_mul(
        token_storage_bytes + one_quote_token_storage_bytes * QUOTE_TOKEN_IDS.len() as u128,
    )
}

const VECTOR_ELEMENT_KEY_BYTES: u128 = 1 + 4;
const HASHED_KEY_BYTES: u128 = 32;

fn storage_cost(bytes: u128) -> NearToken {
    near_sdk::env::storage_byte_cost()
        .checked_mul(bytes)
        .unwrap()
}

fn borsh_len<T: BorshSerialize + ?Sized>(value: &T) -> u128 {
    near_sdk::borsh::object_length(value).unwrap() as u128
}

fn launch_record_storage_bytes(
    token_account_id: &AccountId,
    launch_info: &LaunchInfo,
    new_token_id_counter: Option<(&String, u64)>,
) -> u128 {
    let key_element = STORAGE_ENTRY_COST_BYTES
        + StorageKey::LaunchData.into_storage_key().len() as u128
        + VECTOR_ELEMENT_KEY_BYTES
        + borsh_len(token_account_id);
    let value_element =
        STORAGE_ENTRY_COST_BYTES + HASHED_KEY_BYTES + borsh_len(launch_info) + borsh_len(&0u32);
    let counter_element = new_token_id_counter.map_or(0, |(symbol_lower, id_number)| {
        STORAGE_ENTRY_COST_BYTES
            + StorageKey::IdCounter.into_storage_key().len() as u128
            + borsh_len(symbol_lower)
            + borsh_len(&id_number)
    });
    key_element + value_element + counter_element
}

fn ft_account_record_bytes(account_id_len: u128) -> u128 {
    STORAGE_ENTRY_COST_BYTES + 1 + STRING_PREFIX_BORSH_BYTES + account_id_len + 16
}

fn ft_storage_deposit() -> NearToken {
    storage_cost(ft_account_record_bytes(AccountId::MAX_LEN as u128))
}

fn token_storage_bytes(metadata: &FungibleTokenMetadata) -> u128 {
    const ACCOUNT_RECORD_BYTES: u128 = 100;
    const GLOBAL_CONTRACT_HASH_BYTES: u128 = 32;
    const STATE_RECORD_BYTES: u128 =
        STORAGE_ENTRY_COST_BYTES + b"STATE".len() as u128 + (4 + 1) + 16 + 8 + (4 + 1);
    ACCOUNT_RECORD_BYTES
        + GLOBAL_CONTRACT_HASH_BYTES
        + STATE_RECORD_BYTES
        + (STORAGE_ENTRY_COST_BYTES + 1 + borsh_len(metadata))
        + ft_account_record_bytes(near_sdk::env::current_account_id().len() as u128)
}

fn dex_storage_bytes(asset: &AssetId) -> u128 {
    let element = |prefix_len: u128| {
        (STORAGE_ENTRY_COST_BYTES + prefix_len + VECTOR_ELEMENT_KEY_BYTES + borsh_len(asset))
            + (STORAGE_ENTRY_COST_BYTES + HASHED_KEY_BYTES + 16 + borsh_len(&0u32))
    };
    element(1 + borsh_len(&near_sdk::env::current_account_id()))
        + element(1 + borsh_len(&PLACH_DEX_ID.parse::<DexId>().unwrap()))
        + element(1)
}

fn pool_storage_bytes(
    quote_asset: &AssetId,
    token_asset: &AssetId,
    fees: &FeeConfiguration,
    receivers: &[FeeEntry],
) -> u128 {
    let record = |key_len: u128, value_len: u128| {
        STORAGE_ENTRY_COST_BYTES
            + 1
            + borsh_len(&PLACH_DEX_ID.parse::<DexId>().unwrap())
            + (4 + key_len)
            + (4 + value_len)
    };
    let fee_record = |account_id: &AccountIdRef, asset: &AssetId| {
        record(1 + borsh_len(account_id) + borsh_len(asset), 16)
    };
    // Pool::LaunchV2 { quote_asset, launched_asset, fees, phantom_liquidity }
    let pool = record(
        1 + 4,
        1 + (borsh_len(quote_asset) + 16) + (borsh_len(token_asset) + 16) + borsh_len(fees) + 16,
    );
    let receiver_records: u128 = receivers
        .iter()
        .filter_map(|(receiver, _)| match receiver {
            FeeReceiver::Account(account_id) => {
                Some(fee_record(account_id, quote_asset) + fee_record(account_id, token_asset))
            }
            FeeReceiver::Pool | FeeReceiver::Community(_) => None,
        })
        .sum();
    pool + fee_record(PROTOCOL_FEE_RECEIVER_ID, token_asset) + receiver_records
}

struct QuoteTokenSettings {
    phantom_liquidity: Balance,
}

fn quote_token_settings(quote_token: &AccountId) -> Result<QuoteTokenSettings, String> {
    match quote_token.as_str() {
        USDC_TOKEN_ID => Ok(QuoteTokenSettings {
            phantom_liquidity: 1_500 * 10u128.pow(6),
        }),
        TEAR_TOKEN_ID => Ok(QuoteTokenSettings {
            phantom_liquidity: 100_000 * 10u128.pow(24),
        }),
        _ => Err(format!("{quote_token} can't be a quote token.")),
    }
}

#[near(serializers=[borsh, json])]
#[derive(Clone)]
pub struct LaunchInfo {
    #[serde(flatten)]
    data: LaunchData,
    launched_by: AccountId,
    launched_at_ns: Timestamp,
}

#[near(serializers=[borsh, json])]
#[derive(Clone)]
pub struct LaunchData {
    telegram: Option<String>,
    x: Option<String>,
    twitch: Option<String>,
    website: Option<String>,
    description: Option<String>,
}

impl LaunchData {
    fn errors(&self) -> Vec<String> {
        [
            ("Telegram", &self.telegram, "https://t.me/"),
            ("X", &self.x, "https://x.com/"),
            ("Twitch", &self.twitch, "https://twitch.tv/"),
            ("Website", &self.website, "https://"),
        ]
        .into_iter()
        .filter(|(_, url, prefix)| url.as_ref().is_some_and(|url| !url.starts_with(prefix)))
        .map(|(name, _, prefix)| format!("{name} URL must start with {prefix}."))
        .collect()
    }
}

#[near(event_json(standard = "intear_launch"))]
pub enum IntearLaunchEvent {
    #[event_version("1.0.0")]
    TokenEdited {
        token_account_id: AccountId,
        edited_by: AccountId,
        launch_data: LaunchData,
    },
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    launch_data: IterableMap<AccountId, LaunchInfo>,
    token_id_counter: LookupMap<String, u64>,
    fees_earned: NearToken,
    quote_token_balances: LookupMap<(AccountId, AccountId), U128>,
    /// Paid by accounts that deposit quote tokens, so their balances can't grief storage
    storage_deposits: LookupMap<AccountId, NearToken>,
}

#[near(serializers=[borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    LegacyLaunchData,
    IdCounter,
    LaunchData,
    QuoteTokenBalances,
    StorageDeposits,
}

#[near]
impl Contract {
    #[init]
    pub fn new() -> Self {
        Self {
            launch_data: IterableMap::new(StorageKey::LaunchData),
            token_id_counter: LookupMap::new(StorageKey::IdCounter),
            fees_earned: Default::default(),
            quote_token_balances: LookupMap::new(StorageKey::QuoteTokenBalances),
            storage_deposits: LookupMap::new(StorageKey::StorageDeposits),
        }
    }

    pub fn get_quote_token_balance(&self, account_id: AccountId, quote_token: AccountId) -> U128 {
        self.quote_token_balances
            .get(&(account_id, quote_token))
            .copied()
            .unwrap_or_default()
    }

    #[payable]
    pub fn withdraw_quote_token(&mut self, quote_token: AccountId) -> Promise {
        near_sdk::assert_one_yocto();
        let account_id = near_sdk::env::predecessor_account_id();
        let Some(amount) = self
            .quote_token_balances
            .remove(&(account_id.clone(), quote_token.clone()))
        else {
            panic!("No {quote_token} deposited.");
        };
        Promise::new(quote_token.clone())
            .function_call(
                "ft_transfer",
                near_sdk::serde_json::json!({
                    "receiver_id": account_id,
                    "amount": amount,
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(10),
            )
            .then(
                Self::ext(near_sdk::env::current_account_id())
                    .with_static_gas(Gas::from_tgas(5))
                    .on_quote_token_withdrawn(account_id, quote_token, amount),
            )
    }

    #[private]
    pub fn on_quote_token_withdrawn(
        &mut self,
        account_id: AccountId,
        quote_token: AccountId,
        amount: U128,
        #[callback_result] transfer_result: Result<(), PromiseError>,
    ) {
        if transfer_result.is_err() {
            self.internal_credit_quote_token(account_id, quote_token, amount);
        }
    }

    pub fn short_id_cost(&self) -> NearToken {
        SHORT_ID_COST
    }

    pub fn fees_earned(&self) -> NearToken {
        self.fees_earned
    }

    #[private]
    pub fn withdraw_fees(&mut self, to: AccountId) -> Promise {
        let amount = std::mem::take(&mut self.fees_earned);
        require!(!amount.is_zero(), "No fees to withdraw.");
        Promise::new(to).transfer(amount).then(
            Self::ext(near_sdk::env::current_account_id())
                .with_static_gas(Gas::from_tgas(5))
                .on_fees_withdrawn(amount),
        )
    }

    #[private]
    pub fn on_fees_withdrawn(
        &mut self,
        amount: NearToken,
        #[callback_result] transfer_result: Result<(), PromiseError>,
    ) {
        if transfer_result.is_err() {
            self.fees_earned = self.fees_earned.checked_add(amount).unwrap();
        }
    }

    pub fn get_launch_data(&self, token_account_id: AccountId) -> Option<LaunchInfo> {
        self.launch_data.get(&token_account_id).cloned()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_launch(
        &self,
        account_id: AccountId,
        name: String,
        symbol: String,
        icon: Option<String>,
        decimals: u8,
        total_supply: U128,
        short_id: bool,
        fees: Option<Vec<FeeEntryReference>>,
        launch_data: LaunchData,
        first_buy: Option<U128>,
        quote_token: Option<AccountId>,
    ) -> LaunchPreparation {
        match self.internal_plan_launch(
            account_id,
            name,
            symbol,
            icon,
            decimals,
            total_supply,
            short_id,
            fees,
            launch_data,
            first_buy,
            quote_token,
        ) {
            Ok(plan) => LaunchPreparation {
                token_account_id: Some(plan.token_account_id),
                deposit: Some(plan.deposit),
                errors: Vec::new(),
            },
            Err(LaunchErrors {
                token_account_id,
                errors,
            }) => LaunchPreparation {
                token_account_id,
                deposit: None,
                errors,
            },
        }
    }

    #[payable]
    #[allow(clippy::too_many_arguments)]
    pub fn launch_token(
        &mut self,
        name: String,
        symbol: String,
        icon: Option<String>,
        decimals: u8,
        total_supply: U128,
        short_id: bool,
        fees: Option<Vec<FeeEntryReference>>,
        launch_data: LaunchData,
        first_buy: Option<U128>,
        quote_token: Option<AccountId>,
    ) -> AccountId {
        let LaunchPlan {
            token_account_id: new_token_account_id,
            symbol_lower,
            id_number,
            quote_asset,
            fee_configuration,
            phantom_liquidity,
            metadata,
            launch_info,
            first_buy_near,
            own_storage_bytes,
            token_storage_deposit,
            dex_storage_deposit,
            pool_storage_deposit,
            deposit,
        } = self
            .internal_plan_launch(
                near_sdk::env::predecessor_account_id(),
                name,
                symbol,
                icon,
                decimals,
                total_supply,
                short_id,
                fees,
                launch_data,
                first_buy,
                quote_token.clone(),
            )
            .unwrap_or_else(|launch_errors| panic!("{}", launch_errors.errors.join(" ")));
        let attached_deposit = near_sdk::env::attached_deposit();
        require!(
            attached_deposit == deposit,
            format!(
                "Attach exactly {} (see prepare_launch), not {}.",
                deposit.exact_amount_display(),
                attached_deposit.exact_amount_display()
            )
        );

        if let (Some(quote_token), Some(first_buy)) = (&quote_token, first_buy) {
            let balance_key = (near_sdk::env::predecessor_account_id(), quote_token.clone());
            let balance = self.quote_token_balances.get_mut(&balance_key).unwrap();
            balance.0 -= first_buy.0;
            if balance.0 == 0 {
                self.quote_token_balances.remove(&balance_key);
            }
            self.quote_token_balances.flush();
        }

        let storage_usage_before = near_sdk::env::storage_usage();
        if let Some(id_number) = id_number {
            self.token_id_counter.insert(symbol_lower, id_number);
        }
        self.launch_data
            .insert(new_token_account_id.clone(), launch_info);
        self.launch_data.flush();
        self.token_id_counter.flush();
        require!(
            (near_sdk::env::storage_usage() - storage_usage_before) as u128 == own_storage_bytes,
            "Storage of the launch record doesn't match its cost. This is a bug."
        );

        if short_id {
            self.fees_earned = self.fees_earned.checked_add(SHORT_ID_COST).unwrap();
        }

        let token_init_args = near_sdk::serde_json::json!({
            "owner_id": near_sdk::env::current_account_id(),
            "total_supply": total_supply,
            "metadata": metadata,
        })
        .to_string()
        .into_bytes();
        let token_init_gas = TOKEN_INIT_GAS
            .checked_add(
                TOKEN_INIT_GAS_PER_ARGS_BYTE
                    .checked_mul(token_init_args.len() as u64)
                    .unwrap(),
            )
            .unwrap();

        let create_token_promise = Promise::new(new_token_account_id.clone())
            .create_account()
            .use_global_contract(
                <[u8; 32]>::try_from(near_sdk::bs58::decode(TOKEN_CODE_HASH).into_vec().unwrap())
                    .unwrap(),
            )
            .transfer(token_storage_deposit)
            .function_call("new", token_init_args, NearToken::ZERO, token_init_gas);

        let prepare_dex_promise = Promise::new(INTEAR_DEX_CONTRACT_ID.parse().unwrap())
            .function_call(
                "storage_deposit",
                near_sdk::serde_json::json!({}).to_string().into_bytes(),
                dex_storage_deposit,
                Gas::from_tgas(5),
            )
            .function_call(
                "register_assets",
                near_sdk::serde_json::json!({
                    "asset_ids": [
                        AssetId::Nep141(new_token_account_id.clone()),
                        quote_asset.clone(),
                    ]
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(5),
            )
            .function_call(
                "register_assets",
                near_sdk::serde_json::json!({
                    "asset_ids": [
                        AssetId::Nep141(new_token_account_id.clone()),
                        quote_asset.clone(),
                    ],
                    "for": {
                        "Dex": PLACH_DEX_ID,
                    },
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(5),
            );

        let transfer_to_dex_promise = Promise::new(new_token_account_id.clone())
            .function_call(
                "storage_deposit",
                near_sdk::serde_json::json!({
                    "account_id": INTEAR_DEX_CONTRACT_ID,
                    "registration_only": true,
                })
                .to_string()
                .into_bytes(),
                ft_storage_deposit(),
                Gas::from_tgas(5),
            )
            .function_call(
                "storage_deposit",
                near_sdk::serde_json::json!({
                    "account_id": near_sdk::env::predecessor_account_id(),
                    "registration_only": true,
                })
                .to_string()
                .into_bytes(),
                ft_storage_deposit(),
                Gas::from_tgas(5),
            )
            .function_call(
                "ft_transfer_call",
                near_sdk::serde_json::json!({
                    "receiver_id": INTEAR_DEX_CONTRACT_ID,
                    "amount": total_supply,
                    "memo": null,
                    "msg": "",
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(40),
            );

        let mut operations = vec![Operation::DexCall {
            dex_id: PLACH_DEX_ID.parse().unwrap(),
            method: "create_pool".to_string(),
            args: Base64VecU8(
                near_sdk::borsh::to_vec(&CreatePoolArgs {
                    assets: (
                        quote_asset.clone(),
                        AssetId::Nep141(new_token_account_id.clone()),
                    ),
                    fees: fee_configuration,
                    pool_type: launch_pool_type(phantom_liquidity),
                    initial_liquidity: None,
                })
                .unwrap(),
            ),
            attached_assets: HashMap::from_iter([
                (AssetId::Near, U128(pool_storage_deposit.as_yoctonear())),
                (AssetId::Nep141(new_token_account_id.clone()), total_supply),
            ]),
        }];

        if let Some(first_buy) = first_buy {
            operations.extend([
                Operation::SwapSimple {
                    dex_id: PLACH_DEX_ID.parse().unwrap(),
                    message: Base64VecU8(
                        near_sdk::borsh::to_vec(&SwapArgs {
                            routes: vec![SwapRoute {
                                fraction: FULL_SPLIT_FRACTION,
                                pool_ids: vec![u32::MAX],
                            }],
                            fee_discount: None,
                        })
                        .unwrap(),
                    ),
                    asset_in: quote_asset,
                    asset_out: AssetId::Nep141(new_token_account_id.clone()),
                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(first_buy)),
                    constraint: None,
                },
                Operation::Withdraw {
                    asset_id: AssetId::Nep141(new_token_account_id.clone()),
                    amount: WithdrawAmount::Full { at_least: None },
                    to: Some(near_sdk::env::predecessor_account_id()),
                    rescue_address: None,
                },
            ]);
        }

        // All of the attached NEAR is credited to this contract's balance in Intear DEX before the
        // operations run
        let create_pool_promise = Promise::new(INTEAR_DEX_CONTRACT_ID.parse().unwrap())
            .function_call(
                "execute_operations",
                near_sdk::serde_json::json!({
                    "operations": operations,
                })
                .to_string()
                .into_bytes(),
                pool_storage_deposit
                    .checked_add(first_buy_near.unwrap_or_default())
                    .unwrap(),
                Gas::from_tgas(150),
            );

        let launch_promise = create_token_promise
            .then(prepare_dex_promise)
            .then(transfer_to_dex_promise);
        let launch_promise = match (quote_token, first_buy) {
            (Some(quote_token), Some(first_buy)) => {
                let deposit_first_buy_promise = Promise::new(quote_token).function_call(
                    "ft_transfer_call",
                    near_sdk::serde_json::json!({
                        "receiver_id": INTEAR_DEX_CONTRACT_ID,
                        "amount": first_buy,
                        "memo": null,
                        "msg": "",
                    })
                    .to_string()
                    .into_bytes(),
                    NearToken::from_yoctonear(1),
                    Gas::from_tgas(50),
                );
                launch_promise.then(deposit_first_buy_promise)
            }
            _ => launch_promise,
        };
        launch_promise.then(create_pool_promise).detach();

        new_token_account_id
    }

    #[payable]
    pub fn edit_token(&mut self, token_account_id: AccountId, launch_data: LaunchData) {
        let errors = launch_data.errors();
        require!(errors.is_empty(), errors.join(" "));
        let attached_deposit = near_sdk::env::attached_deposit();
        require!(
            !attached_deposit.is_zero(),
            "Attach at least 1 yoctoNEAR to edit a token."
        );
        let Some(launch_info) = self.launch_data.get_mut(&token_account_id) else {
            panic!("Token not found");
        };
        require!(
            launch_info.launched_by == near_sdk::env::predecessor_account_id(),
            "Only token creator can edit own tokens"
        );
        let storage_usage_before = near_sdk::env::storage_usage();
        launch_info.data = launch_data;
        let edited_launch_data = launch_info.data.clone();
        self.launch_data.flush();
        IntearLaunchEvent::TokenEdited {
            token_account_id,
            edited_by: near_sdk::env::predecessor_account_id(),
            launch_data: edited_launch_data,
        }
        .emit();
        let storage_usage_after = near_sdk::env::storage_usage();
        let leftover = if storage_usage_after >= storage_usage_before {
            let storage_cost = storage_cost((storage_usage_after - storage_usage_before).into());
            let Some(leftover) = attached_deposit.checked_sub(storage_cost) else {
                panic!(
                    "Insufficient deposit for storage cost. Attach at least {}.",
                    storage_cost.exact_amount_display()
                );
            };
            leftover
        } else {
            attached_deposit
                .checked_add(storage_cost(
                    (storage_usage_before - storage_usage_after).into(),
                ))
                .unwrap()
        };
        if !leftover.is_zero() {
            Promise::new(near_sdk::env::predecessor_account_id())
                .transfer(leftover)
                .detach();
        }
    }
}

#[near]
impl FungibleTokenReceiver for Contract {
    /// Takes quote tokens for first buys of later launches, from accounts registered with
    /// storage_deposit
    fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        let quote_token = near_sdk::env::predecessor_account_id();
        if let Err(error) = quote_token_settings(&quote_token) {
            panic!("{error}");
        }
        require!(msg.is_empty(), "msg must be empty.");
        if !self.storage_deposits.contains_key(&sender_id) {
            panic!("Register {sender_id} with storage_deposit before depositing {quote_token}.");
        }
        self.internal_credit_quote_token(sender_id, quote_token, amount);
        PromiseOrValue::Value(U128(0))
    }
}

/// Every account pays the same deposit, which covers its balance in every quote token, so
/// there's never anything available to withdraw.
#[near]
impl StorageManagement for Contract {
    /// Refunds the whole deposit if the account is already registered
    #[payable]
    #[allow(unused_variables)]
    fn storage_deposit(
        &mut self,
        account_id: Option<AccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        let account_id = account_id.unwrap_or_else(near_sdk::env::predecessor_account_id);
        let attached_deposit = near_sdk::env::attached_deposit();
        let refund = if self.storage_deposits.contains_key(&account_id) {
            attached_deposit
        } else {
            let Some(refund) = attached_deposit.checked_sub(quote_token_storage_deposit()) else {
                panic!(
                    "Insufficient deposit for storage cost. Attach at least {}",
                    quote_token_storage_deposit()
                );
            };
            self.storage_deposits
                .insert(account_id.clone(), quote_token_storage_deposit());
            refund
        };
        if !refund.is_zero() {
            Promise::new(near_sdk::env::predecessor_account_id())
                .transfer(refund)
                .detach();
        }
        self.storage_balance_of(account_id).unwrap()
    }

    #[payable]
    fn storage_withdraw(&mut self, amount: Option<NearToken>) -> StorageBalance {
        near_sdk::assert_one_yocto();
        let account_id = near_sdk::env::predecessor_account_id();
        let Some(storage_balance) = self.storage_balance_of(account_id.clone()) else {
            panic!("{account_id} is not registered.");
        };
        if amount.is_some_and(|amount| !amount.is_zero()) {
            panic!("The whole storage deposit is in use, and registration is permanent.");
        }
        storage_balance
    }

    #[payable]
    #[allow(unused_variables)]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        panic!("Unregistering is not supported.");
    }

    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        StorageBalanceBounds {
            min: quote_token_storage_deposit(),
            max: Some(quote_token_storage_deposit()),
        }
    }

    fn storage_balance_of(&self, account_id: AccountId) -> Option<StorageBalance> {
        self.storage_deposits
            .get(&account_id)
            .map(|&total| StorageBalance {
                total,
                available: NearToken::ZERO,
            })
    }
}

#[near(serializers = [json])]
pub struct LaunchPreparation {
    /// The token's account, if the symbol gives one that this launch can create
    pub token_account_id: Option<AccountId>,
    /// The exact deposit to attach to launch_token, if there are no errors
    pub deposit: Option<NearToken>,
    /// Why launch_token would fail
    pub errors: Vec<String>,
}

struct LaunchErrors {
    token_account_id: Option<AccountId>,
    errors: Vec<String>,
}

/// Everything a launch creates, from its validated arguments, and the exact deposit it takes
struct LaunchPlan {
    token_account_id: AccountId,
    symbol_lower: String,
    id_number: Option<u64>,
    quote_asset: AssetId,
    fee_configuration: FeeConfiguration,
    phantom_liquidity: Balance,
    metadata: FungibleTokenMetadata,
    launch_info: LaunchInfo,
    first_buy_near: Option<NearToken>,
    own_storage_bytes: u128,
    token_storage_deposit: NearToken,
    dex_storage_deposit: NearToken,
    pool_storage_deposit: NearToken,
    deposit: NearToken,
}

fn launch_pool_type(phantom_liquidity: Balance) -> PoolType {
    PoolType::LaunchV2 {
        phantom_liquidity: U128(phantom_liquidity),
    }
}

impl Contract {
    /// Finds every argument that would make a later receipt of the launch fail
    #[allow(clippy::too_many_arguments)]
    fn internal_plan_launch(
        &self,
        launched_by: AccountId,
        name: String,
        symbol: String,
        icon: Option<String>,
        decimals: u8,
        total_supply: U128,
        short_id: bool,
        fees: Option<Vec<FeeEntryReference>>,
        launch_data: LaunchData,
        first_buy: Option<U128>,
        quote_token: Option<AccountId>,
    ) -> Result<LaunchPlan, LaunchErrors> {
        let mut errors = launch_data.errors();
        if total_supply.0 == 0 {
            errors.push("Total supply must be positive.".to_string());
        }
        if first_buy.is_some_and(|amount| amount.0 == 0) {
            errors.push("First buy must be positive.".to_string());
        }
        // The token refunds the storage deposit of an account that is already registered
        if launched_by.as_str() == INTEAR_DEX_CONTRACT_ID
            || launched_by == near_sdk::env::current_account_id()
        {
            errors.push(format!("{launched_by} can't launch tokens."));
        }

        let quote = match &quote_token {
            None => Some((AssetId::Near, NearToken::from_near(300).as_yoctonear())),
            Some(quote_token) => match quote_token_settings(quote_token) {
                Ok(settings) => Some((
                    AssetId::Nep141(quote_token.clone()),
                    settings.phantom_liquidity,
                )),
                Err(error) => {
                    errors.push(error);
                    None
                }
            },
        };
        if let Some((_, phantom_liquidity)) = quote
            && phantom_liquidity
                .checked_add(first_buy.map_or(0, |amount| amount.0))
                .is_none()
        {
            errors.push("First buy is too large.".to_string());
        }
        if let (Some(quote_token), Some(first_buy), Some(_)) = (&quote_token, first_buy, &quote) {
            match self
                .quote_token_balances
                .get(&(launched_by.clone(), quote_token.clone()))
            {
                None => errors.push(format!(
                    "Deposit {quote_token} with ft_transfer_call for the first buy first."
                )),
                Some(balance) if balance.0 < first_buy.0 => errors.push(format!(
                    "First buy is {} of {quote_token}, but only {} is deposited.",
                    first_buy.0, balance.0
                )),
                Some(_) => {}
            }
        }

        let receivers = fees
            .unwrap_or_default()
            .into_iter()
            .map(|(receiver, amount)| {
                (
                    match receiver {
                        FeeReceiverReference::Account(receiver_account_id) => {
                            FeeReceiver::Account(receiver_account_id)
                        }
                        FeeReceiverReference::Pool => FeeReceiver::Pool,
                    },
                    amount,
                )
            })
            .collect::<Vec<FeeEntry>>();
        let fee_configuration = FeeConfiguration::V2(V2FeeConfiguration {
            receivers: receivers.clone(),
        });
        // Fees only go down over time, so they're also valid when the pool is created
        if let Some((_, phantom_liquidity)) = quote
            && let Err(error) = fee_configuration.validate(
                &launch_pool_type(phantom_liquidity),
                near_sdk::env::block_timestamp(),
            )
        {
            errors.push(format!("Invalid fees: {error}"));
        }
        if !receivers.iter().enumerate().all(|(index, (receiver, _))| {
            receivers[..index]
                .iter()
                .all(|(other, _)| other != receiver)
        }) {
            errors.push("Each fee receiver can be listed only once.".to_string());
        }

        let token_account = match self.new_token_account_id(&symbol, short_id) {
            Ok(token_account) => Some(token_account),
            Err(error) => {
                errors.push(error);
                None
            }
        };
        if let Some((token_account_id, _)) = &token_account
            && storage_cost(dex_storage_bytes(&AssetId::Nep141(
                token_account_id.clone(),
            ))) < INTEAR_DEX_MIN_STORAGE_DEPOSIT
        {
            errors.push(
                "Intear DEX storage of this token is below its minimum deposit. Use a longer symbol."
                    .to_string(),
            );
        }

        if !errors.is_empty() {
            return Err(LaunchErrors {
                token_account_id: token_account.map(|(token_account_id, _)| token_account_id),
                errors,
            });
        }
        let (Some((token_account_id, id_number)), Some((quote_asset, phantom_liquidity))) =
            (token_account, quote)
        else {
            unreachable!("They are missing only with an error");
        };
        let symbol_lower = symbol.to_lowercase();
        let new_token_id_counter =
            id_number.filter(|_| !self.token_id_counter.contains_key(&symbol_lower));
        let metadata = FungibleTokenMetadata {
            spec: "ft-1.0.0".to_string(),
            name,
            symbol,
            icon,
            reference: None,
            reference_hash: None,
            decimals,
        };
        let launch_info = LaunchInfo {
            data: launch_data,
            launched_by,
            launched_at_ns: near_sdk::env::block_timestamp(),
        };
        let token_asset = AssetId::Nep141(token_account_id.clone());

        let own_storage_bytes = launch_record_storage_bytes(
            &token_account_id,
            &launch_info,
            new_token_id_counter.map(|token_id| (&symbol_lower, token_id)),
        );
        let token_storage_deposit = storage_cost(token_storage_bytes(&metadata));
        let dex_storage_deposit = storage_cost(dex_storage_bytes(&token_asset));
        let pool_storage_deposit = storage_cost(pool_storage_bytes(
            &quote_asset,
            &token_asset,
            &fee_configuration,
            &receivers,
        ));
        let first_buy_near = first_buy
            .filter(|_| quote_token.is_none())
            .map(|amount| NearToken::from_yoctonear(amount.0));
        // 2 register_assets and the ft_transfer_call of the supply, and of a quote token first buy
        let attached_yocto = if quote_token.is_some() && first_buy.is_some() {
            4
        } else {
            3
        };
        let deposit = [
            storage_cost(own_storage_bytes),
            token_storage_deposit,
            // dex.intear.near and the creator on the token
            ft_storage_deposit().checked_mul(2).unwrap(),
            dex_storage_deposit,
            pool_storage_deposit,
            NearToken::from_yoctonear(attached_yocto),
            if short_id {
                SHORT_ID_COST
            } else {
                NearToken::ZERO
            },
            first_buy_near.unwrap_or_default(),
        ]
        .into_iter()
        .try_fold(NearToken::ZERO, NearToken::checked_add)
        .unwrap();

        Ok(LaunchPlan {
            token_account_id,
            symbol_lower,
            id_number,
            quote_asset,
            fee_configuration,
            phantom_liquidity,
            metadata,
            launch_info,
            first_buy_near,
            own_storage_bytes,
            token_storage_deposit,
            dex_storage_deposit,
            pool_storage_deposit,
            deposit,
        })
    }

    /// The new token's account ID, and for a long ID, the symbol's new counter
    fn new_token_account_id(
        &self,
        symbol: &str,
        short_id: bool,
    ) -> Result<(AccountId, Option<u64>), String> {
        let symbol_lower = symbol.to_lowercase();
        let (account_id, id_number) = if short_id {
            if symbol.contains("-") {
                return Err("Symbol cannot contain hyphens when using a short ID.".to_string());
            }
            (
                format!("{symbol_lower}.{}", near_sdk::env::current_account_id()),
                None,
            )
        } else {
            let id_number = self
                .token_id_counter
                .get(&symbol_lower)
                .copied()
                .unwrap_or_default()
                + 1;
            (
                format!(
                    "{symbol_lower}-{id_number}.{}",
                    near_sdk::env::current_account_id()
                ),
                Some(id_number),
            )
        };
        let Ok(token_account_id) = account_id.parse::<AccountId>() else {
            return Err("Invalid ticker".to_string());
        };
        // This contract can only create its direct sub-accounts
        if !token_account_id.is_sub_account_of(near_sdk::env::current_account_id()) {
            return Err("Symbol cannot contain dots.".to_string());
        }
        if self.launch_data.contains_key(&token_account_id) {
            return Err(format!("{token_account_id} is already taken."));
        }
        Ok((token_account_id, id_number))
    }

    fn internal_credit_quote_token(
        &mut self,
        account_id: AccountId,
        quote_token: AccountId,
        amount: U128,
    ) {
        let balance = self
            .quote_token_balances
            .entry((account_id, quote_token))
            .or_default();
        balance.0 = balance.0.checked_add(amount.0).unwrap();
    }
}

#[near(serializers=[json])]
#[derive(PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
pub enum FeeReceiverReference {
    Account(AccountId),
    Pool,
}

pub type FeeEntryReference = (FeeReceiverReference, FeeAmount);
pub type FeeEntry = (FeeReceiver, FeeAmount);
