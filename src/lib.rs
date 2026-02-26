use std::collections::HashMap;

use near_contract_standards::fungible_token::metadata::FungibleTokenMetadata;
use near_sdk::{
    AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise, Timestamp,
    json_types::{Base64VecU8, U128},
    near, require,
    store::LookupMap,
};

const INTEAR_DEX_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(5); // 0.005 NEAR
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

const TOKEN_CODE_HASH: &str = "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF";
const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const PHANTOM_LIQUIDITY_NEAR: NearToken = NearToken::from_near(300);

#[near(serializers=[borsh, json])]
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

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    launch_data: LookupMap<AccountId, LaunchInfo>,
    meme_id_counter: LookupMap<String, u64>,
    fees_earned: NearToken,
}

#[near(serializers=[borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    LegacyLaunchData,
    IdCounter,
    LaunchData,
}

#[near]
impl Contract {
    #[init]
    pub fn new() -> Self {
        Self {
            launch_data: LookupMap::new(StorageKey::LaunchData),
            meme_id_counter: LookupMap::new(StorageKey::IdCounter),
            fees_earned: Default::default(),
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

    pub fn get_launch_data(&self, token_account_id: AccountId) -> Option<&LaunchInfo> {
        self.launch_data.get(&token_account_id)
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
        fees: Option<Vec<FeeEntry>>,
        launch_data: LaunchData,
        first_buy: Option<NearToken>,
    ) -> AccountId {
        launch_data.validate();
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
            .and_then(|deposit| deposit.checked_sub(first_buy.unwrap_or_default()))
        else {
            panic!("Insufficient deposit for launch cost. Attach at least {cost}.");
        };

        let account_id = if short_id {
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

        let create_token_promise = Promise::new(account_id.clone())
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
                Gas::from_tgas(10),
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
                        AssetId::Nep141(account_id.clone()),
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
                        AssetId::Nep141(account_id.clone()),
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

        let transfer_to_dex_promise = Promise::new(account_id.clone())
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

        #[near(serializers=[borsh])]
        struct CreatePoolArgs {
            assets: (AssetId, AssetId),
            fees: FeeConfiguration,
            pool_type: PoolType,
        }
        let mut operations = vec![Operation::DexCall {
            dex_id: PLACH_DEX_ID.to_string(),
            method: "create_pool".to_string(),
            args: Base64VecU8(
                near_sdk::borsh::to_vec(&CreatePoolArgs {
                    assets: (AssetId::Near, AssetId::Nep141(account_id.clone())),
                    fees: FeeConfiguration::V2(V2FeeConfiguration {
                        receivers: fees.unwrap_or_default(),
                    }),
                    pool_type: PoolType::LaunchV1 {
                        phantom_liquidity_near: U128(PHANTOM_LIQUIDITY_NEAR.as_yoctonear()),
                    },
                })
                .unwrap(),
            ),
            attached_assets: HashMap::from_iter([
                (
                    AssetId::Near,
                    U128(PLACH_POOL_STORAGE_DEPOSIT.as_yoctonear()),
                ),
                (AssetId::Nep141(account_id.clone()), total_supply),
            ]),
        }];

        if let Some(first_buy) = first_buy {
            #[near(serializers=[borsh])]
            struct SwapArgs {
                pool_id: u32,
            }
            operations.extend([
                Operation::SwapSimple {
                    dex_id: PLACH_DEX_ID.to_string(),
                    message: Base64VecU8(
                        near_sdk::borsh::to_vec(&SwapArgs { pool_id: u32::MAX }).unwrap(),
                    ),
                    asset_in: AssetId::Near,
                    asset_out: AssetId::Nep141(account_id.clone()),
                    amount: SwapOperationAmount::Amount(SwapRequestAmount::ExactIn(U128(
                        first_buy.as_yoctonear(),
                    ))),
                    constraint: None,
                },
                Operation::Withdraw {
                    asset_id: AssetId::Nep141(account_id.clone()),
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
                if let Some(first_buy) = first_buy {
                    first_buy
                } else {
                    NearToken::from_yoctonear(1)
                },
                Gas::from_tgas(150),
            );

        create_token_promise
            .then(prepare_dex_promise)
            .then(transfer_to_dex_promise)
            .then(create_pool_promise)
            .detach();

        account_id
    }
}

#[derive(near_sdk::serde::Serialize)]
#[serde(crate = "near_sdk::serde")]
pub enum Operation {
    DexCall {
        dex_id: String,
        method: String,
        args: Base64VecU8,
        attached_assets: HashMap<AssetId, U128>,
    },
    Withdraw {
        asset_id: AssetId,
        amount: WithdrawAmount,
        to: Option<AccountId>,
        /// If the withdrawal fails and current user doesn't have
        /// a registerd balance in this asset, the assets will be
        /// refunded to this address. It's required that either
        /// the user address or rescue address is registered.
        rescue_address: Option<AccountId>,
    },
    SwapSimple {
        dex_id: String,
        message: Base64VecU8,
        asset_in: AssetId,
        asset_out: AssetId,
        amount: SwapOperationAmount,
        /// Either minimum amount out (for ExactIn) or maximum amount in (for ExactOut)
        constraint: Option<U128>,
    },
}

#[derive(near_sdk::serde::Serialize)]
#[serde(crate = "near_sdk::serde")]
pub enum SwapOperationAmount {
    Amount(SwapRequestAmount),
    OutputOfLastIn,
    EntireBalanceIn,
}

#[derive(near_sdk::serde::Serialize)]
#[serde(crate = "near_sdk::serde")]
pub enum SwapRequestAmount {
    ExactIn(U128),
    ExactOut(U128),
}

#[derive(near_sdk::serde::Serialize)]
#[serde(crate = "near_sdk::serde")]
pub enum WithdrawAmount {
    Full { at_least: Option<U128> },
    Exact(U128),
    PreviousSwapOutput,
}

#[near(serializers=[borsh])]
#[derive(PartialEq, Eq, Hash)]
pub enum AssetId {
    Near,
    Nep141(AccountId),
    Nep245(AccountId, String),
    Nep171(AccountId, String),
}

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Near => write!(f, "near"),
            Self::Nep141(contract_id) => write!(f, "nep141:{contract_id}"),
            Self::Nep245(contract_id, token_id) => write!(f, "nep245:{contract_id}:{token_id}"),
            Self::Nep171(contract_id, token_id) => write!(f, "nep171:{contract_id}:{token_id}"),
        }
    }
}

impl near_sdk::serde::Serialize for AssetId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: near_sdk::serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

#[near(serializers=[borsh])]
enum PoolType {
    PrivateLatest,
    PublicLatest,
    LaunchLatest { phantom_liquidity_near: U128 },
    LaunchV1 { phantom_liquidity_near: U128 },
    PrivateV1,
    PublicV1,
    PrivateV2,
    PublicV2,
}

#[near(serializers=[borsh, json])]
enum FeeConfiguration {
    V1(/* not supported */),
    V2(V2FeeConfiguration),
}

pub type FeeEntry = (FeeReceiver, FeeAmount);

#[near(serializers=[borsh, json])]
struct V2FeeConfiguration {
    receivers: Vec<FeeEntry>,
}

#[near(serializers=[borsh, json])]
#[derive(PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
pub enum FeeReceiver {
    Account(AccountId),
    Pool,
}

#[near(serializers=[borsh, json])]
#[derive(Clone, Copy)]
pub enum FeeAmount {
    Fixed(u32),
    Scheduled {
        start: (u64, u32),
        end: (u64, u32),
        curve: ScheduledFeeCurve,
    },
    Dynamic {
        min: u32,
        max: u32,
    },
}

#[near(serializers=[borsh, json])]
#[derive(Clone, Copy)]
pub enum ScheduledFeeCurve {
    Linear,
}
