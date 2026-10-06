#![no_std]
//! TrustEscrow factory. Deploys one escrow instance per trade and holds the
//! operator configuration copied into each new escrow.
//!
//! Configuration changes never reach existing escrows: each instance stores its
//! own arbitrator and fee at creation and has no setter.

use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    panic_with_error, xdr::ToXdr, Address, Bytes, BytesN, Env,
};

pub use trustescrow_types::{EscrowParams, Order, MAX_FEE_BPS};

/// Just enough of the escrow's interface to call `fund` on one the factory
/// just deployed. A minimal trait rather than depending on the escrow crate,
/// so the factory's WASM never links the escrow's implementation.
#[contractclient(name = "EscrowClient")]
#[allow(dead_code)]
trait EscrowFund {
    fn fund(env: Env);
}

const DAY_IN_LEDGERS: u32 = 17_280;
const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
const TTL_EXTEND_TO: u32 = 120 * DAY_IN_LEDGERS;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub escrow_wasm_hash: BytesN<32>,
    pub arbitrator: Address,
    pub fee_recipient: Address,
    pub fee_bps: u32,
}

/// Bounds on `Order::amount` for one allowlisted token. Both ends inclusive.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenLimits {
    pub min_amount: i128,
    pub max_amount: i128,
}

#[contracttype]
enum DataKey {
    Config,
    Token(Address),
    PendingAdmin,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    TokenNotAllowed = 1,
    InvalidFee = 2,
    AdminChangeRequiresTransfer = 3,
    NoPendingAdmin = 4,
    InvalidTokenLimits = 5,
    AmountTooSmall = 6,
    AmountTooLarge = 7,
}

#[contractevent(topics = ["escrow"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowCreated {
    #[topic]
    pub buyer: Address,
    #[topic]
    pub seller: Address,
    pub escrow: Address,
    pub token: Address,
    pub amount: i128,
}

#[contractevent(topics = ["config"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigUpdated {
    pub config: Config,
}

// `limits` isn't `Option<TokenLimits>`: soroban-sdk 27's `#[contractevent]`
// hits the same XDR-derive limitation as `#[contracttype]` does for
// `Option<T>` over a contract type (see crates/types). `min_amount` and
// `max_amount` are 0 when `allowed` is false.
#[contractevent(topics = ["token"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenAllowed {
    #[topic]
    pub token: Address,
    pub allowed: bool,
    pub min_amount: i128,
    pub max_amount: i128,
}

#[contractevent(topics = ["adm_prop"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminProposed {
    #[topic]
    pub current: Address,
    #[topic]
    pub proposed: Address,
}

#[contractevent(topics = ["adm_xfer"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminTransferred {
    #[topic]
    pub previous: Address,
    #[topic]
    pub admin: Address,
}

#[contractevent(topics = ["adm_cncl"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminTransferCancelled {
    #[topic]
    pub current: Address,
    #[topic]
    pub cancelled: Address,
}

#[contract]
pub struct Factory;

#[contractimpl]
impl Factory {
    pub fn __constructor(env: Env, config: Config) {
        write_config(&env, &config);
    }

    /// Deploy a new escrow for `order`. The buyer authorises creation; the
    /// escrow address is derived from the buyer and `salt`, so it is known
    /// before submission and no one else can claim it.
    pub fn create(env: Env, order: Order, salt: BytesN<32>) -> Address {
        order.buyer.require_auth();
        let limits = Self::token_limits(env.clone(), order.token.clone())
            .unwrap_or_else(|| panic_with_error!(&env, Error::TokenNotAllowed));
        if order.amount < limits.min_amount {
            panic_with_error!(&env, Error::AmountTooSmall);
        }
        if order.amount > limits.max_amount {
            panic_with_error!(&env, Error::AmountTooLarge);
        }
        let config = Self::config(env.clone());
        let params = EscrowParams {
            order: order.clone(),
            arbitrator: config.arbitrator,
            fee_bps: config.fee_bps,
            fee_recipient: config.fee_recipient,
            salt: salt.clone(),
        };

        let escrow = env
            .deployer()
            .with_current_contract(escrow_salt(&env, &order.buyer, &salt))
            .deploy_v2(config.escrow_wasm_hash, (params,));
        extend_instance_ttl(&env);

    /// `create`, then `fund()` on the new escrow in the same transaction —
    /// one signature covering create -> fund -> token transfer, instead of
    /// leaving an unfunded escrow if the buyer never comes back to fund it.
    pub fn create_and_fund(env: Env, order: Order, salt: BytesN<32>) -> Address {
        let escrow = do_create(&env, order, salt);
        EscrowClient::new(&env, &escrow).fund();
        escrow
    }

    /// The address `create` will deploy to for this buyer and salt.
    pub fn escrow_address(env: Env, buyer: Address, salt: BytesN<32>) -> Address {
        env.deployer()
            .with_current_contract(escrow_salt(&env, &buyer, &salt))
            .deployed_address()
    }

    /// Replace the configuration used for escrows created from now on. The
    /// admin cannot be changed here; use `propose_admin` and `accept_admin`.
    pub fn set_config(env: Env, config: Config) {
        let current = Self::config(env.clone());
        current.admin.require_auth();
        if config.admin != current.admin {
            panic_with_error!(&env, Error::AdminChangeRequiresTransfer);
        }
        write_config(&env, &config);
        ConfigUpdated { config }.publish(&env);
    }

    /// Start handing the factory to `proposed`. Nothing changes until the
    /// proposed address accepts, so a mistyped address can never lock the
    /// factory. Proposing again replaces the pending proposal.
    pub fn propose_admin(env: Env, proposed: Address) {
        let current = Self::config(env.clone()).admin;
        current.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &proposed);
        extend_instance_ttl(&env);
        AdminProposed { current, proposed }.publish(&env);
    }

    /// The proposed admin takes over, proving it can sign.
    pub fn accept_admin(env: Env) {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingAdmin));
        pending.require_auth();

        let mut config = Self::config(env.clone());
        let previous = config.admin;
        config.admin = pending.clone();
        write_config(&env, &config);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        AdminTransferred {
            previous,
            admin: pending,
        }
        .publish(&env);
    }

    /// Withdraw a pending proposal.
    pub fn cancel_admin_transfer(env: Env) {
        let current = Self::config(env.clone()).admin;
        current.require_auth();
        let cancelled: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingAdmin));
        env.storage().instance().remove(&DataKey::PendingAdmin);
        extend_instance_ttl(&env);
        AdminTransferCancelled { current, cancelled }.publish(&env);
    }

    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Allow `token` for orders between `limits.min_amount` and
    /// `limits.max_amount` (inclusive), or disallow it with `None`.
    pub fn allow_token(env: Env, token: Address, limits: Option<TokenLimits>) {
        Self::config(env.clone()).admin.require_auth();
        let key = DataKey::Token(token.clone());
        let (allowed, min_amount, max_amount) = match limits {
            Some(limits) => {
                if limits.min_amount <= 0 || limits.min_amount > limits.max_amount {
                    panic_with_error!(&env, Error::InvalidTokenLimits);
                }
                env.storage().persistent().set(&key, &limits);
                extend_persistent_ttl(&env, &key);
                (true, limits.min_amount, limits.max_amount)
            }
            None => {
                env.storage().persistent().remove(&key);
                (false, 0, 0)
            }
        };
        TokenAllowed {
            token,
            allowed,
            min_amount,
            max_amount,
        }
        .publish(&env);
    }

    pub fn config(env: Env) -> Config {
        env.storage().instance().get(&DataKey::Config).unwrap()
    }

    pub fn is_token_allowed(env: Env, token: Address) -> bool {
        Self::token_limits(env, token).is_some()
    }

    pub fn token_limits(env: Env, token: Address) -> Option<TokenLimits> {
        let key = DataKey::Token(token);
        let limits: Option<TokenLimits> = env.storage().persistent().get(&key);
        if limits.is_some() {
            extend_persistent_ttl(&env, &key);
        }
        limits
    }
}

fn do_create(env: &Env, order: Order, salt: BytesN<32>) -> Address {
    order.buyer.require_auth();
    if !Factory::is_token_allowed(env.clone(), order.token.clone()) {
        panic_with_error!(env, Error::TokenNotAllowed);
    }
    let config = Factory::config(env.clone());
    let params = EscrowParams {
        order: order.clone(),
        arbitrator: config.arbitrator,
        fee_bps: config.fee_bps,
        fee_recipient: config.fee_recipient,
        salt: salt.clone(),
    };

    let escrow = env
        .deployer()
        .with_current_contract(escrow_salt(env, &order.buyer, &salt))
        .deploy_v2(config.escrow_wasm_hash, (params,));
    extend_instance_ttl(env);

    EscrowCreated {
        buyer: order.buyer,
        seller: order.seller,
        escrow: escrow.clone(),
        token: order.token,
        amount: order.amount,
    }
    .publish(env);
    escrow
}

fn write_config(env: &Env, config: &Config) {
    if config.fee_bps > MAX_FEE_BPS {
        panic_with_error!(env, Error::InvalidFee);
    }
    env.storage().instance().set(&DataKey::Config, config);
    extend_instance_ttl(env);
}

fn escrow_salt(env: &Env, buyer: &Address, salt: &BytesN<32>) -> BytesN<32> {
    let mut preimage = buyer.clone().to_xdr(env);
    preimage.append(&Bytes::from(salt.clone()));
    env.crypto().sha256(&preimage).into()
}

fn ttl_bounds(env: &Env) -> (u32, u32) {
    let extend_to = TTL_EXTEND_TO.min(env.storage().max_ttl());
    (TTL_THRESHOLD.min(extend_to), extend_to)
}

fn extend_instance_ttl(env: &Env) {
    let (threshold, extend_to) = ttl_bounds(env);
    env.storage().instance().extend_ttl(threshold, extend_to);
}

fn extend_persistent_ttl(env: &Env, key: &DataKey) {
    let (threshold, extend_to) = ttl_bounds(env);
    env.storage()
        .persistent()
        .extend_ttl(key, threshold, extend_to);
}

#[cfg(test)]
mod test;
