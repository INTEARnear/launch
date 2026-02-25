use std::collections::HashMap;

use near_contract_standards::fungible_token::metadata::FungibleTokenMetadata;
use near_sdk::{
    AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise,
    json_types::{Base64VecU8, U128},
    near, require,
    store::LookupMap,
};

const INTEAR_DEX_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(5); // 0.005 NEAR
const PLACH_POOL_STORAGE_DEPOSIT: NearToken = NearToken::from_millinear(10); // 0.01 NEAR
const FT_STORAGE_DEPOSIT: NearToken = NearToken::from_micronear(1250); // 0.00125 NEAR
const OWN_STORAGE_EXPENSES: NearToken = NearToken::from_millinear(10); // 0.01 NEAR

// 0.02625 NEAR
const ID_COST: NearToken = NearToken::from_yoctonear(
    INTEAR_DEX_STORAGE_DEPOSIT.as_yoctonear()
        + PLACH_POOL_STORAGE_DEPOSIT.as_yoctonear()
        + OWN_STORAGE_EXPENSES.as_yoctonear()
        + FT_STORAGE_DEPOSIT.as_yoctonear(),
);
const SHORT_ID_COST: NearToken = NearToken::from_near(1);

const TOKEN_CODE_HASH: &str = "8D1NEU2NC2hKhdtCkHyyAz2KVmVXRazm9ZQMC27D97jF";
const INTEAR_DEX_CONTRACT_ID: &str = "dex.intear.near";
const PLACH_DEX_ID: &str = "slimedragon.near/xyk";
const PHANTOM_LIQUIDITY_NEAR: NearToken = NearToken::from_near(300);

#[near(serializers=[borsh, json])]
pub struct LaunchData {
    telegram: Option<String>,
    x: Option<String>,
    website: Option<String>,
}

impl LaunchData {
    fn validate(&self) {
        const MAX_LENGTH: usize = 100;
        require!(
            self.telegram
                .as_ref()
                .is_none_or(|url| url.len() <= MAX_LENGTH),
            "Telegram URL must be less than {MAX_LENGTH} characters."
        );
        require!(
            self.x.as_ref().is_none_or(|url| url.len() <= MAX_LENGTH),
            "X URL must be less than {MAX_LENGTH} characters."
        );
        require!(
            self.website
                .as_ref()
                .is_none_or(|url| url.len() <= MAX_LENGTH),
            "Website URL must be less than {MAX_LENGTH} characters."
        );
    }
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    launch_data: LookupMap<AccountId, LaunchData>,
    meme_id_counter: LookupMap<String, u64>,
    fees_earned: NearToken,
}

#[near(serializers=[borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    LaunchData,
    IdCounter,
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
                .expect("Invalid account ID to be created. Try using a shorter ticker.");
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
            .expect("Invalid account ID to be created. Try using a longer ticker.")
        }
    }

    pub fn get_launch_data(&self, token_account_id: AccountId) -> Option<&LaunchData> {
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

        let Some(storage_deposit) = near_sdk::env::attached_deposit().checked_sub(cost) else {
            panic!("Insufficient deposit for launch cost. Attach at least {cost}.");
        };

        let account_id = if short_id {
            require!(
                !symbol.contains("-"),
                "Symbol cannot contain hyphens when using a short ID."
            );
            let account_id = format!("{symbol_lower}.{}", near_sdk::env::current_account_id())
                .parse::<AccountId>()
                .expect("Invalid account ID to be created. Try using a shorter ticker.");
            if self
                .launch_data
                .insert(account_id.clone(), launch_data)
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
            .expect("Invalid account ID to be created. Try using a longer ticker.");
            if self
                .launch_data
                .insert(account_id.clone(), launch_data)
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
                Gas::from_tgas(30),
            );

        let prepare_dex_promise = Promise::new(INTEAR_DEX_CONTRACT_ID.parse().unwrap())
            .function_call(
                "storage_deposit",
                near_sdk::serde_json::json!({}).to_string().into_bytes(),
                INTEAR_DEX_STORAGE_DEPOSIT,
                Gas::from_tgas(10),
            )
            .function_call(
                "register_assets",
                near_sdk::serde_json::json!({
                    "asset_ids": [
                        format!("nep141:{account_id}")
                    ]
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(10),
            )
            .function_call(
                "register_assets",
                near_sdk::serde_json::json!({
                    "asset_ids": [
                        format!("nep141:{account_id}"),
                    ],
                    "for": {
                        "Dex": PLACH_DEX_ID,
                    },
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(10),
            )
            .function_call(
                "deposit_near",
                near_sdk::serde_json::json!({}).to_string().into_bytes(),
                PLACH_POOL_STORAGE_DEPOSIT,
                Gas::from_tgas(10),
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
                Gas::from_tgas(10),
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

        let create_pool_promise = Promise::new(INTEAR_DEX_CONTRACT_ID.parse().unwrap())
            .function_call(
                "execute_operations",
                near_sdk::serde_json::json!({
                    "operations": [
                        Operation::DexCall {
                            dex_id: PLACH_DEX_ID.to_string(),
                            method: "create_pool".to_string(),
                            args: Base64VecU8(
                                near_sdk::borsh::to_vec(&CreatePoolArgs {
                                    assets: (AssetId::Near, AssetId::Nep141(account_id.clone())),
                                    fees: FeeConfiguration::V2(V2FeeConfiguration {
                                        receivers: fees.unwrap_or_default()
                                    }),
                                    pool_type: PoolType::LaunchV1 {
                                        phantom_liquidity_near: U128(PHANTOM_LIQUIDITY_NEAR.as_yoctonear())
                                    },
                                })
                                .unwrap(),
                            ),
                            attached_assets: HashMap::from_iter([
                                (
                                    "near".to_string(),
                                    U128(PLACH_POOL_STORAGE_DEPOSIT.as_yoctonear()),
                                ),
                                (
                                    format!("nep141:{account_id}").to_string(),
                                    total_supply,
                                ),
                            ]),
                        },
                    ]
                }).to_string().into_bytes(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(120),
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
        attached_assets: HashMap<String, U128>,
    },
}

#[near(serializers=[borsh])]
struct CreatePoolArgs {
    assets: (AssetId, AssetId),
    fees: FeeConfiguration,
    pool_type: PoolType,
}

#[near(serializers=[borsh])]
pub enum AssetId {
    Near,
    Nep141(AccountId),
    Nep245(AccountId, String),
    Nep171(AccountId, String),
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
