use std::collections::HashMap;

use intear_dex_types::{
    AssetId, Operation, SwapOperationAmount, SwapRequestAmount, WithdrawAmount,
};
use near_contract_standards::{
    fungible_token::{Balance, metadata::FungibleTokenMetadata, receiver::FungibleTokenReceiver},
    storage_management::{StorageBalance, StorageBalanceBounds, StorageManagement},
};
use near_sdk::{
    AccountId, BorshStorageKey, Gas, IntoStorageKey, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue, Timestamp,
    json_types::{Base64VecU8, U128},
    near, require,
    store::{IterableMap, LookupMap},
};
use xyk_dex_types::{
    CreatePoolArgs, FULL_SPLIT_FRACTION, FeeAmount, FeeConfiguration, FeeReceiver, PoolType,
    SwapArgs, SwapRoute, V2FeeConfiguration,
};

const INTEAR_DEX_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10); // 0.01 NEAR
const PLACH_POOL_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(15); // 0.015 NEAR
const FT_STORAGE_DEPOSIT: NearToken = NearToken::from_micronear(1250); // 0.00125 NEAR
const OWN_STORAGE_EXPENSES: NearToken = NearToken::from_millinear(10); // 0.01 NEAR

// 0.03250 NEAR
const ID_COST: NearToken = NearToken::from_yoctonear(
    INTEAR_DEX_STORAGE_DEPOSIT.as_yoctonear()
        + PLACH_POOL_STORAGE_DEPOSIT.as_yoctonear()
        + OWN_STORAGE_EXPENSES.as_yoctonear()
        + 2 * FT_STORAGE_DEPOSIT.as_yoctonear(),
);
const SHORT_ID_COST: NearToken = NearToken::from_near(1);

// https://github.com/INTEARnear/global-ft-contract
const TOKEN_CODE_HASH: &str = "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF";
const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
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

struct QuoteTokenSettings {
    phantom_liquidity: Balance,
}

fn quote_token_settings(quote_token: &AccountId) -> QuoteTokenSettings {
    match quote_token.as_str() {
        USDC_TOKEN_ID => QuoteTokenSettings {
            phantom_liquidity: 1_500 * 10u128.pow(6),
        },
        TEAR_TOKEN_ID => QuoteTokenSettings {
            phantom_liquidity: 100_000 * 10u128.pow(24),
        },
        _ => panic!("{quote_token} can't be a quote token."),
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
    fn validate(&self) {
        const MAX_URL_LENGTH: usize = 50;
        require!(
            self.telegram
                .as_ref()
                .is_none_or(|url| url.len() <= MAX_URL_LENGTH),
            "Telegram URL must be less than {MAX_URL_LENGTH} characters."
        );
        require!(
            self.telegram.as_ref().is_none_or(|url| {
                url.strip_prefix("https://t.me/")
                    .is_some_and(|handle| !handle.contains('/'))
            }),
            "Telegram handle must not contain '/'."
        );
        require!(
            self.x
                .as_ref()
                .is_none_or(|url| url.len() <= MAX_URL_LENGTH),
            "X URL must be less than {MAX_URL_LENGTH} characters."
        );
        require!(
            self.x.as_ref().is_none_or(|url| {
                url.strip_prefix("https://x.com/")
                    .is_some_and(|handle| !handle.contains('/'))
            }),
            "X handle must not contain '/'."
        );
        require!(
            self.twitch
                .as_ref()
                .is_none_or(|url| url.len() <= MAX_URL_LENGTH),
            "Twitch URL must be less than {MAX_URL_LENGTH} characters."
        );
        require!(
            self.twitch.as_ref().is_none_or(|url| {
                url.strip_prefix("https://twitch.tv/")
                    .is_some_and(|handle| !handle.is_empty() && !handle.contains('/'))
            }),
            "Twitch URL must be https://twitch.tv/<handle>."
        );
        require!(
            self.website
                .as_ref()
                .is_none_or(|url| url.len() <= MAX_URL_LENGTH),
            "Website URL must be less than {MAX_URL_LENGTH} characters."
        );
        require!(
            self.website
                .as_ref()
                .is_none_or(|url| url.starts_with("https://")),
            "Website URL must start with https://."
        );
        const MAX_DESCRIPTION_LENGTH: usize = 200;
        require!(
            self.description
                .as_ref()
                .is_none_or(|desc| desc.len() <= MAX_DESCRIPTION_LENGTH),
            "Description must be less than {MAX_DESCRIPTION_LENGTH} characters."
        );
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
    meme_id_counter: LookupMap<String, u64>,
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
            meme_id_counter: LookupMap::new(StorageKey::IdCounter),
            fees_earned: Default::default(),
            quote_token_balances: LookupMap::new(StorageKey::QuoteTokenBalances),
            storage_deposits: LookupMap::new(StorageKey::StorageDeposits),
        }
    }

    #[init(ignore_state)]
    #[private]
    pub fn migrate() -> Self {
        #[near(serializers=[borsh])]
        struct OldContract {
            launch_data: IterableMap<AccountId, LaunchInfo>,
            meme_id_counter: LookupMap<String, u64>,
            fees_earned: NearToken,
            quote_token_balances: LookupMap<(AccountId, AccountId), U128>,
        }
        let old_state: OldContract = near_sdk::env::state_read().expect("Failed to read old state");
        Self {
            launch_data: old_state.launch_data,
            meme_id_counter: old_state.meme_id_counter,
            fees_earned: old_state.fees_earned,
            quote_token_balances: old_state.quote_token_balances,
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

    pub fn long_id_cost(&self) -> NearToken {
        ID_COST
    }

    pub fn fees_earned(&self) -> NearToken {
        self.fees_earned
    }

    #[private]
    pub fn withdraw_fees(&mut self, to: AccountId) {
        Promise::new(to).transfer(self.fees_earned).detach();
        self.fees_earned = NearToken::ZERO;
    }

    pub fn preview_id(&self, symbol: String, short_id: bool) -> AccountId {
        let symbol_lower = symbol.to_lowercase();
        if short_id {
            require!(
                !symbol.contains("-"),
                "Symbol cannot contain hyphens when using a short ID"
            );
            let account_id = format!("{symbol_lower}.{}", near_sdk::env::current_account_id())
                .parse::<AccountId>()
                .expect("Invalid ticker");
            if self.launch_data.contains_key(&account_id) {
                panic!("Short account ID for this symbol is already taken.");
            }
            account_id
        } else {
            let next_meme_id = self
                .meme_id_counter
                .get(&symbol_lower)
                .copied()
                .unwrap_or_default()
                + 1;
            format!(
                "{symbol_lower}-{next_meme_id}.{}",
                near_sdk::env::current_account_id()
            )
            .parse::<AccountId>()
            .expect("Invalid ticker")
        }
    }

    pub fn get_launch_data(&self, token_account_id: AccountId) -> Option<LaunchInfo> {
        self.launch_data.get(&token_account_id).cloned()
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
        launch_data.validate();
        let (quote_asset, phantom_liquidity) = match &quote_token {
            None => (AssetId::Near, NearToken::from_near(300).as_yoctonear()),
            Some(quote_token) => (
                AssetId::Nep141(quote_token.clone()),
                quote_token_settings(quote_token).phantom_liquidity,
            ),
        };
        let first_buy_near = if quote_asset == AssetId::Near {
            first_buy.map(|amount| NearToken::from_yoctonear(amount.0))
        } else {
            None
        };
        if let (Some(quote_token), Some(first_buy)) = (&quote_token, first_buy) {
            let balance_key = (near_sdk::env::predecessor_account_id(), quote_token.clone());
            let Some(balance) = self.quote_token_balances.get_mut(&balance_key) else {
                panic!("Deposit {quote_token} with ft_transfer_call for the first buy first.");
            };
            let Some(balance_left) = balance.0.checked_sub(first_buy.0) else {
                panic!(
                    "First buy is {} of {quote_token}, but only {} is deposited.",
                    first_buy.0, balance.0
                );
            };
            if balance_left == 0 {
                self.quote_token_balances.remove(&balance_key);
            } else {
                balance.0 = balance_left;
            }
            self.quote_token_balances.flush();
        }
        let symbol_lower = symbol.to_lowercase();

        let own_storage_allowed = u64::try_from(
            OWN_STORAGE_EXPENSES.as_yoctonear() / near_sdk::env::storage_byte_cost().as_yoctonear(),
        )
        .unwrap();
        let storage_usage_before = near_sdk::env::storage_usage();

        let cost = if short_id {
            SHORT_ID_COST.checked_add(ID_COST).unwrap()
        } else {
            ID_COST
        };

        let Some(storage_deposit) = near_sdk::env::attached_deposit()
            .checked_sub(cost)
            .and_then(|deposit| deposit.checked_sub(first_buy_near.unwrap_or_default()))
        else {
            panic!("Insufficient deposit for launch cost. Attach at least {cost}.");
        };

        let new_token_account_id = if short_id {
            require!(
                !symbol.contains("-"),
                "Symbol cannot contain hyphens when using a short ID."
            );
            let account_id = format!("{symbol_lower}.{}", near_sdk::env::current_account_id())
                .parse::<AccountId>()
                .expect("Invalid ticker");
            if self
                .launch_data
                .insert(
                    account_id.clone(),
                    LaunchInfo {
                        data: launch_data,
                        launched_by: near_sdk::env::predecessor_account_id(),
                        launched_at_ns: near_sdk::env::block_timestamp(),
                    },
                )
                .is_some()
            {
                panic!("Short account ID for this symbol is already taken");
            }
            account_id
        } else {
            let next_meme_id = self
                .meme_id_counter
                .get(&symbol_lower)
                .copied()
                .unwrap_or_default()
                + 1;
            self.meme_id_counter
                .insert(symbol_lower.clone(), next_meme_id);
            let account_id = format!(
                "{symbol_lower}-{next_meme_id}.{}",
                near_sdk::env::current_account_id()
            )
            .parse::<AccountId>()
            .expect("Invalid ticker");
            if self
                .launch_data
                .insert(
                    account_id.clone(),
                    LaunchInfo {
                        data: launch_data,
                        launched_by: near_sdk::env::predecessor_account_id(),
                        launched_at_ns: near_sdk::env::block_timestamp(),
                    },
                )
                .is_some()
            {
                panic!("Long account ID for this symbol is already taken. This is a bug.");
            }
            account_id
        };

        self.launch_data.flush();
        self.meme_id_counter.flush();
        let storage_usage_after = near_sdk::env::storage_usage();
        let storage_usage = storage_usage_after
            .checked_sub(storage_usage_before)
            .unwrap();
        require!(
            storage_usage <= own_storage_allowed,
            "Insufficient deposit for storage cost. Attach at least {storage_cost}."
        );

        if short_id {
            self.fees_earned = self.fees_earned.checked_add(SHORT_ID_COST).unwrap();
        }

        let create_token_promise = Promise::new(new_token_account_id.clone())
            .create_account()
            .use_global_contract(
                <[u8; 32]>::try_from(near_sdk::bs58::decode(TOKEN_CODE_HASH).into_vec().unwrap())
                    .unwrap(),
            )
            .transfer(storage_deposit)
            .function_call(
                "new",
                near_sdk::serde_json::json!({
                    "owner_id": near_sdk::env::current_account_id(),
                    "total_supply": total_supply,
                    "metadata": FungibleTokenMetadata {
                        spec: "ft-1.0.0".to_string(),
                        name,
                        symbol,
                        icon,
                        reference: None,
                        reference_hash: None,
                        decimals,
                    }
                })
                .to_string()
                .into_bytes(),
                NearToken::ZERO,
                Gas::from_tgas(35),
            );

        let prepare_dex_promise = Promise::new(INTEAR_DEX_CONTRACT_ID.parse().unwrap())
            .function_call(
                "storage_deposit",
                near_sdk::serde_json::json!({}).to_string().into_bytes(),
                INTEAR_DEX_STORAGE_DEPOSIT,
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
            )
            .function_call(
                "deposit_near",
                near_sdk::serde_json::json!({}).to_string().into_bytes(),
                PLACH_POOL_STORAGE_DEPOSIT,
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
                FT_STORAGE_DEPOSIT,
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
                FT_STORAGE_DEPOSIT,
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
                    fees: FeeConfiguration::V2(V2FeeConfiguration {
                        receivers: fees
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
                            .collect(),
                    }),
                    pool_type: PoolType::LaunchV2 {
                        phantom_liquidity: U128(phantom_liquidity),
                    },
                    initial_liquidity: None,
                })
                .unwrap(),
            ),
            attached_assets: HashMap::from_iter([
                (
                    AssetId::Near,
                    U128(PLACH_POOL_STORAGE_DEPOSIT.as_yoctonear()),
                ),
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

        let create_pool_promise = Promise::new(INTEAR_DEX_CONTRACT_ID.parse().unwrap())
            .function_call(
                "execute_operations",
                near_sdk::serde_json::json!({
                    "operations": operations,
                })
                .to_string()
                .into_bytes(),
                first_buy_near.unwrap_or(NearToken::from_yoctonear(1)),
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
        launch_data.validate();
        let attached_deposit = near_sdk::env::attached_deposit();
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
        let storage_usage_change = storage_usage_after.saturating_sub(storage_usage_before) as u128;
        let storage_change_cost = NearToken::from_yoctonear(
            storage_usage_change
                .checked_mul(near_sdk::env::storage_byte_cost().as_yoctonear())
                .unwrap(),
        );
        let Some(leftover) = attached_deposit.checked_sub(storage_change_cost) else {
            panic!("Insufficient deposit for storage cost. Attach at least {storage_change_cost}.");
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
        // Panics for tokens that can't be quote tokens
        quote_token_settings(&quote_token);
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
            panic!(
                "The whole storage deposit is in use. Unregister with storage_unregister instead."
            );
        }
        storage_balance
    }

    #[payable]
    #[allow(unused_variables)]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        near_sdk::assert_one_yocto();
        let account_id = near_sdk::env::predecessor_account_id();
        if QUOTE_TOKEN_IDS.iter().any(|quote_token| {
            self.quote_token_balances
                .contains_key(&(account_id.clone(), quote_token.parse().unwrap()))
        }) && !force.unwrap_or_default()
        {
            panic!(
                "Withdraw deposited quote tokens with withdraw_quote_token before unregistering."
            );
        }
        let Some(storage_deposit) = self.storage_deposits.remove(&account_id) else {
            return false;
        };
        Promise::new(account_id).transfer(storage_deposit).detach();
        true
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

impl Contract {
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
