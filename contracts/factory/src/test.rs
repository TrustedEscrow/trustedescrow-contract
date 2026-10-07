#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{
        storage::{Instance as _, Persistent as _},
        Address as _, Events, Ledger,
    },
    token::{StellarAssetClient, TokenClient},
    xdr::ContractEvent,
    Bytes, BytesN, Env, Event, IntoVal, String,
};

mod escrow_wasm {
    soroban_sdk::contractimport!(
        file = "../../target/wasm32v1-none/release/trustescrow_escrow.wasm"
    );
}

const START: u64 = 1_700_000_000;
const DAY: u64 = 86_400;
const AMOUNT: i128 = 500_000_000;
const FEE_BPS: u32 = 150;
const CODE: &[u8] = b"K7M29XQF4TBNR3WD";

struct Setup<'a> {
    env: Env,
    admin: Address,
    arbitrator: Address,
    fee_recipient: Address,
    token: Address,
    factory: FactoryClient<'a>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let admin = Address::generate(&env);
    let arbitrator = Address::generate(&env);
    let fee_recipient = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let escrow_wasm_hash = env.deployer().upload_contract_wasm(escrow_wasm::WASM);

    let id = env.register(
        Factory,
        (Config {
            admin: admin.clone(),
            escrow_wasm_hash,
            arbitrator: arbitrator.clone(),
            fee_recipient: fee_recipient.clone(),
            fee_bps: FEE_BPS,
        },),
    );
    let factory = FactoryClient::new(&env, &id);
    factory.allow_token(
        &token,
        &Some(TokenLimits {
            min_amount: 1,
            max_amount: i128::MAX,
        }),
    );

    Setup {
        env,
        admin,
        arbitrator,
        fee_recipient,
        token,
        factory,
    }
}

/// Entry points panic with a contract error rather than returning `Result`, so
/// the generated `try_` client surfaces it as a generic `soroban_sdk::Error`.
fn assert_err<T: core::fmt::Debug, E: core::fmt::Debug>(
    result: Result<T, Result<soroban_sdk::Error, E>>,
    expected: Error,
) {
    match result {
        Err(Ok(got)) => assert_eq!(got, soroban_sdk::Error::from(expected)),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

impl Setup<'_> {
    fn order(&self, buyer: &Address) -> Order {
        Order {
            buyer: buyer.clone(),
            seller: Address::generate(&self.env),
            token: self.token.clone(),
            amount: AMOUNT,
            terms_hash: BytesN::from_array(&self.env, &[1; 32]),
            release_code_hash: self
                .env
                .crypto()
                .sha256(&Bytes::from_slice(&self.env, CODE))
                .into(),
            funding_deadline: START + DAY,
            delivery_window: 7 * DAY,
            receipt_window: 3 * DAY,
            arbitration_window: 30 * DAY,
        }
    }

    fn salt(&self, n: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[n; 32])
    }

    /// Limits wide enough that `AMOUNT` (and any amount a test picks) always
    /// clears them — tests that care about the bounds set their own.
    fn wide_limits(&self) -> Option<TokenLimits> {
        Some(TokenLimits {
            min_amount: 1,
            max_amount: i128::MAX,
        })
    }

    fn escrow(&self, address: &Address) -> escrow_wasm::Client<'_> {
        escrow_wasm::Client::new(&self.env, address)
    }

    /// Events emitted by `contract` during the last invocation, in order.
    fn events_of(&self, contract: &Address) -> std::vec::Vec<ContractEvent> {
        self.env
            .events()
            .all()
            .filter_by_contract(contract)
            .events()
            .to_vec()
    }

    fn ttl(&self) -> u32 {
        self.env
            .as_contract(&self.factory.address, || self.env.storage().instance().get_ttl())
    }

    fn token_ttl(&self, token: &Address) -> u32 {
        self.env.as_contract(&self.factory.address, || {
            self.env
                .storage()
                .persistent()
                .get_ttl(&DataKey::Token(token.clone()))
        })
    }

    fn advance_ledgers(&self, ledgers: u32) {
        let sequence = self.env.ledger().sequence();
        self.env.ledger().set_sequence_number(sequence + ledgers);
    }

    fn event(&self, event: &impl Event) -> ContractEvent {
        event.to_xdr(&self.env, &self.factory.address)
    }
}

#[test]
fn create_deploys_escrow_at_predicted_address_with_factory_config() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    let predicted = s.factory.escrow_address(&buyer, &s.salt(1));

    let address = s.factory.create(&order, &s.salt(1));
    let auths = s.env.auths();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].0, buyer);
    assert_eq!(address, predicted);

    let e = s.escrow(&address).get();
    assert_eq!(e.buyer, buyer);
    assert_eq!(e.seller, order.seller);
    assert_eq!(e.arbitrator, s.arbitrator);
    assert_eq!(e.fee_bps, FEE_BPS);
    assert_eq!(e.fee_recipient, s.fee_recipient);
    assert_eq!(e.release_code_hash, order.release_code_hash);
    assert_eq!(e.state, escrow_wasm::State::Created);

    // The provenance check a client runs: the escrow's own stored salt,
    // fed back through the factory, must reproduce the escrow's address.
    assert_eq!(e.salt, s.salt(1));
    assert_eq!(s.factory.escrow_address(&buyer, &e.salt), address);
}

#[test]
fn create_and_fund_deploys_and_funds_in_one_call() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    StellarAssetClient::new(&s.env, &s.token).mint(&buyer, &AMOUNT);
    let predicted = s.factory.escrow_address(&buyer, &s.salt(1));

    let address = s.factory.create_and_fund(&order, &s.salt(1));
    assert_eq!(address, predicted);

    let escrow = s.escrow(&address);
    assert_eq!(escrow.get().state, escrow_wasm::State::Funded);
    assert_eq!(TokenClient::new(&s.env, &s.token).balance(&address), AMOUNT);
    assert_eq!(TokenClient::new(&s.env, &s.token).balance(&buyer), 0);
}

/// `create_and_fund` must ask the buyer for exactly one signature, whose
/// authorised tree covers `create_and_fund` -> the escrow's `fund` ->
/// the token `transfer` — not three separate signatures, and not a
/// signature the wallet can't see the full shape of.
#[test]
fn create_and_fund_asks_for_exactly_one_signature_covering_the_whole_chain() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    StellarAssetClient::new(&s.env, &s.token).mint(&buyer, &AMOUNT);

    s.factory.create_and_fund(&order, &s.salt(1));

    let auths = s.env.auths();
    assert_eq!(auths.len(), 1, "expected one signer, got {auths:?}");
    assert_eq!(auths[0].0, buyer);

    let root = &auths[0].1;
    assert_eq!(
        root.sub_invocations.len(),
        1,
        "expected one sub-call (fund)"
    );
    let fund_call = &root.sub_invocations[0];
    assert_eq!(
        fund_call.sub_invocations.len(),
        1,
        "expected one sub-call under fund (the token transfer)"
    );
    assert!(
        fund_call.sub_invocations[0].sub_invocations.is_empty(),
        "the token transfer shouldn't need any further authorisation"
    );
}

#[test]
fn directly_deployed_escrow_fails_the_factory_provenance_check() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    let claimed_salt = s.salt(1);

    // Deploy the very same audited WASM, but go around the factory and hand
    // the constructor whatever it likes: its own arbitrator, the maximum
    // fee, and a salt copied from a real order so it *looks* legitimate.
    let rogue_arbitrator = Address::generate(&s.env);
    let rogue_fee_recipient = Address::generate(&s.env);
    let fake_params = escrow_wasm::EscrowParams {
        order: escrow_wasm::Order {
            buyer: buyer.clone(),
            seller: order.seller.clone(),
            token: order.token.clone(),
            amount: order.amount,
            terms_hash: order.terms_hash.clone(),
            release_code_hash: order.release_code_hash.clone(),
            funding_deadline: order.funding_deadline,
            delivery_window: order.delivery_window,
            receipt_window: order.receipt_window,
            arbitration_window: order.arbitration_window,
        },
        arbitrator: rogue_arbitrator,
        fee_bps: MAX_FEE_BPS,
        fee_recipient: rogue_fee_recipient,
        salt: claimed_salt.clone(),
    };
    let rogue = s.env.register(escrow_wasm::WASM, (fake_params,));

    // It runs the pinned WASM and claims a plausible salt, but it was never
    // deployed by the factory: its address isn't derived from the factory's
    // own address, so the check a client runs fails, whatever salt it claims.
    assert_ne!(s.factory.escrow_address(&buyer, &claimed_salt), rogue);
}

#[test]
fn factory_escrow_runs_the_two_sided_flow() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    StellarAssetClient::new(&s.env, &s.token).mint(&buyer, &AMOUNT);

    let escrow = s.escrow(&s.factory.create(&order, &s.salt(1)));
    escrow.fund();
    escrow.submit_proof(
        &escrow_wasm::ProofKind::Tracking,
        &String::from_str(&s.env, "https://track.example/ABC123"),
        &BytesN::from_array(&s.env, &[7; 32]),
    );
    escrow.release_with_code(&Bytes::from_slice(&s.env, CODE));

    let token = TokenClient::new(&s.env, &s.token);
    let fee = AMOUNT * FEE_BPS as i128 / 10_000;
    assert_eq!(token.balance(&order.seller), AMOUNT - fee);
    assert_eq!(token.balance(&s.fee_recipient), fee);
    assert_eq!(token.balance(&escrow.address), 0);
    assert_eq!(escrow.get().state, escrow_wasm::State::Released);
}

#[test]
fn create_rejects_token_not_on_allowlist() {
    let s = setup();
    let mut order = s.order(&Address::generate(&s.env));
    order.token = s
        .env
        .register_stellar_asset_contract_v2(Address::generate(&s.env))
        .address();
    assert_err(
        s.factory.try_create(&order, &s.salt(1)),
        Error::TokenNotAllowed,
    );
}

#[test]
fn amount_at_either_limit_is_accepted() {
    let s = setup();
    s.factory.allow_token(
        &s.token,
        &Some(TokenLimits {
            min_amount: 100,
            max_amount: 200,
        }),
    );
    let mut low = s.order(&Address::generate(&s.env));
    low.amount = 100;
    s.factory.create(&low, &s.salt(1));

    let mut high = s.order(&Address::generate(&s.env));
    high.amount = 200;
    s.factory.create(&high, &s.salt(2));
}

#[test]
fn amount_outside_the_limits_is_rejected() {
    let s = setup();
    s.factory.allow_token(
        &s.token,
        &Some(TokenLimits {
            min_amount: 100,
            max_amount: 200,
        }),
    );
    let mut too_small = s.order(&Address::generate(&s.env));
    too_small.amount = 99;
    assert_err(
        s.factory.try_create(&too_small, &s.salt(1)),
        Error::AmountTooSmall,
    );

    let mut too_large = s.order(&Address::generate(&s.env));
    too_large.amount = 201;
    assert_err(
        s.factory.try_create(&too_large, &s.salt(2)),
        Error::AmountTooLarge,
    );
}

#[test]
fn invalid_limits_are_rejected() {
    let s = setup();
    assert_err(
        s.factory.try_allow_token(
            &s.token,
            &Some(TokenLimits {
                min_amount: 0,
                max_amount: 100,
            }),
        ),
        Error::InvalidTokenLimits,
    );
    assert_err(
        s.factory.try_allow_token(
            &s.token,
            &Some(TokenLimits {
                min_amount: -1,
                max_amount: 100,
            }),
        ),
        Error::InvalidTokenLimits,
    );
    assert_err(
        s.factory.try_allow_token(
            &s.token,
            &Some(TokenLimits {
                min_amount: 101,
                max_amount: 100,
            }),
        ),
        Error::InvalidTokenLimits,
    );
    // The existing, valid limits from setup() are untouched.
    assert_eq!(s.factory.token_limits(&s.token), s.wide_limits());
}

#[test]
fn removed_token_can_no_longer_be_used() {
    let s = setup();
    s.factory.allow_token(&s.token, &None);
    assert!(!s.factory.is_token_allowed(&s.token));
    let order = s.order(&Address::generate(&s.env));
    assert_err(
        s.factory.try_create(&order, &s.salt(1)),
        Error::TokenNotAllowed,
    );
}

#[test]
fn invalid_order_is_rejected_by_the_escrow() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let mut order = s.order(&buyer);
    order.seller = buyer;
    assert!(s.factory.try_create(&order, &s.salt(1)).is_err());
}

#[test]
fn salts_are_scoped_to_the_buyer() {
    let s = setup();
    let (a, b) = (Address::generate(&s.env), Address::generate(&s.env));
    let escrow_a = s.factory.create(&s.order(&a), &s.salt(1));
    let escrow_b = s.factory.create(&s.order(&b), &s.salt(1));
    assert_ne!(escrow_a, escrow_b);
    assert!(s.factory.try_create(&s.order(&a), &s.salt(1)).is_err());
}

#[test]
fn config_changes_apply_only_to_new_escrows() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let first = s.factory.create(&s.order(&buyer), &s.salt(1));

    let new_arbitrator = Address::generate(&s.env);
    let mut config = s.factory.config();
    config.arbitrator = new_arbitrator.clone();
    config.fee_bps = 300;
    s.factory.set_config(&config);
    let auths = s.env.auths();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].0, s.admin);

    let second = s.factory.create(&s.order(&buyer), &s.salt(2));

    let old = s.escrow(&first).get();
    assert_eq!(old.arbitrator, s.arbitrator);
    assert_eq!(old.fee_bps, FEE_BPS);
    let new = s.escrow(&second).get();
    assert_eq!(new.arbitrator, new_arbitrator);
    assert_eq!(new.fee_bps, 300);
}

#[test]
fn fee_above_cap_is_rejected() {
    let s = setup();
    let mut config = s.factory.config();
    config.fee_bps = MAX_FEE_BPS + 1;
    assert_err(s.factory.try_set_config(&config), Error::InvalidFee);
}

#[test]
fn admin_signs_allowlist_changes() {
    let s = setup();
    let token = Address::generate(&s.env);
    s.factory.allow_token(&token, &s.wide_limits());
    let auths = s.env.auths();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].0, s.admin);
    assert!(s.factory.is_token_allowed(&token));
}

// --- Events ------------------------------------------------------------------

#[test]
fn create_emits_escrow_created_and_the_escrow_emits_its_own() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    let escrow = s.factory.create(&order, &s.salt(1));

    let expected = EscrowCreated {
        buyer,
        seller: order.seller,
        escrow: escrow.clone(),
        token: s.token.clone(),
        amount: AMOUNT,
    };
    assert_eq!(
        s.events_of(&s.factory.address),
        std::vec![s.event(&expected)]
    );
    // The new escrow's constructor publishes its own `created` event in the
    // same invocation.
    assert_eq!(s.events_of(&escrow).len(), 1);
}

#[test]
fn create_and_fund_emits_escrow_created_and_the_escrow_emits_created_then_funded() {
    let s = setup();
    let buyer = Address::generate(&s.env);
    let order = s.order(&buyer);
    StellarAssetClient::new(&s.env, &s.token).mint(&buyer, &AMOUNT);
    let escrow = s.factory.create_and_fund(&order, &s.salt(1));

    let expected = EscrowCreated {
        buyer,
        seller: order.seller,
        escrow: escrow.clone(),
        token: s.token.clone(),
        amount: AMOUNT,
    };
    assert_eq!(
        s.events_of(&s.factory.address),
        std::vec![s.event(&expected)]
    );
    // The escrow's constructor and its fund() both publish, in order, in
    // the same top-level invocation.
    assert_eq!(s.events_of(&escrow).len(), 2);
}

#[test]
fn set_config_emits_config_updated() {
    let s = setup();
    let mut config = s.factory.config();
    config.fee_bps = 200;
    s.factory.set_config(&config);
    let expected = ConfigUpdated { config };
    assert_eq!(
        s.events_of(&s.factory.address),
        std::vec![s.event(&expected)]
    );
}

#[test]
fn allowlist_changes_emit_token_allowed() {
    let s = setup();
    for limits in [None, s.wide_limits()] {
        s.factory.allow_token(&s.token, &limits);
        let expected = match limits {
            Some(l) => TokenAllowed {
                token: s.token.clone(),
                allowed: true,
                min_amount: l.min_amount,
                max_amount: l.max_amount,
            },
            None => TokenAllowed {
                token: s.token.clone(),
                allowed: false,
                min_amount: 0,
                max_amount: 0,
            },
        };
        assert_eq!(
            s.events_of(&s.factory.address),
            std::vec![s.event(&expected)]
        );
    }
}

#[test]
fn rejected_create_emits_nothing() {
    let s = setup();
    s.factory.allow_token(&s.token, &None);
    let order = s.order(&Address::generate(&s.env));
    assert!(s.factory.try_create(&order, &s.salt(1)).is_err());
    assert!(s.events_of(&s.factory.address).is_empty());
}

// --- Admin transfer ----------------------------------------------------------

fn assert_signed_by(s: &Setup, who: &Address) {
    let auths = s.env.auths();
    assert_eq!(auths.len(), 1);
    assert_eq!(&auths[0].0, who);
}

#[test]
fn set_config_cannot_change_the_admin() {
    let s = setup();
    let mut config = s.factory.config();
    config.admin = Address::generate(&s.env);
    assert_err(
        s.factory.try_set_config(&config),
        Error::AdminChangeRequiresTransfer,
    );
    assert_eq!(s.factory.config().admin, s.admin);
}

#[test]
fn admin_transfer_takes_effect_only_on_acceptance() {
    let s = setup();
    let next = Address::generate(&s.env);

    s.factory.propose_admin(&next);
    assert_signed_by(&s, &s.admin);
    assert_eq!(s.factory.config().admin, s.admin);
    assert_eq!(s.factory.pending_admin(), Some(next.clone()));

    s.factory.accept_admin();
    assert_signed_by(&s, &next);
    assert_eq!(s.factory.config().admin, next);
    assert_eq!(s.factory.pending_admin(), None);

    // The new admin now signs configuration changes.
    s.factory
        .allow_token(&Address::generate(&s.env), &s.wide_limits());
    assert_signed_by(&s, &next);
}

#[test]
fn accept_without_a_proposal_is_rejected() {
    let s = setup();
    assert_err(s.factory.try_accept_admin(), Error::NoPendingAdmin);
    assert_err(s.factory.try_cancel_admin_transfer(), Error::NoPendingAdmin);
}

#[test]
fn proposals_can_be_replaced_and_withdrawn() {
    let s = setup();
    let (first, second) = (Address::generate(&s.env), Address::generate(&s.env));
    s.factory.propose_admin(&first);
    s.factory.propose_admin(&second);
    assert_eq!(s.factory.pending_admin(), Some(second.clone()));

    s.factory.cancel_admin_transfer();
    assert_signed_by(&s, &s.admin);
    let expected = AdminTransferCancelled {
        current: s.admin.clone(),
        cancelled: second,
    };
    assert_eq!(
        s.events_of(&s.factory.address),
        std::vec![s.event(&expected)]
    );

    assert_eq!(s.factory.pending_admin(), None);
    assert_err(s.factory.try_accept_admin(), Error::NoPendingAdmin);
    assert_eq!(s.factory.config().admin, s.admin);
}

#[test]
fn admin_transfer_emits_proposed_then_transferred() {
    let s = setup();
    let next = Address::generate(&s.env);

    s.factory.propose_admin(&next);
    let proposed = AdminProposed {
        current: s.admin.clone(),
        proposed: next.clone(),
    };
    assert_eq!(
        s.events_of(&s.factory.address),
        std::vec![s.event(&proposed)]
    );

    s.factory.accept_admin();
    let transferred = AdminTransferred {
        previous: s.admin.clone(),
        admin: next,
    };
    assert_eq!(
        s.events_of(&s.factory.address),
        std::vec![s.event(&transferred)]
    );
}

// --- Multisig admin -----------------------------------------------------------
//
// mock_all_auths/mock_auths swap in a stub __check_auth that approves
// unconditionally — they prove nothing about whether a custom account's real
// signature verification runs correctly. These tests sign a real
// authorization entry with real ed25519 keys and submit it with
// env.set_auths, so __check_auth genuinely executes. See
// contracts/escrow/src/test.rs's "Multisig arbitrator" section, where this
// same pattern was built and verified first.

mod multisig {
    use soroban_sdk::{
        auth::{Context, CustomAccountInterface},
        contract, contracterror, contractimpl, contracttype,
        crypto::Hash,
        BytesN, Env, Vec,
    };

    #[contracttype]
    pub enum DataKey {
        Signers,
        Threshold,
    }

    #[contracterror]
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    #[repr(u32)]
    pub enum Error {
        NotEnoughSignatures = 1,
        SignaturesOutOfOrder = 2,
    }

    /// Minimal N-of-M multisig custom account: `threshold` of the `signers`
    /// ed25519 keys (by index into `signers`, strictly increasing so the
    /// same key can't be counted twice) must each produce a valid signature
    /// over the exact payload the host asks `__check_auth` to verify.
    #[contract]
    pub struct MultisigAccount;

    #[contractimpl]
    impl MultisigAccount {
        pub fn __constructor(env: Env, signers: Vec<BytesN<32>>, threshold: u32) {
            env.storage().instance().set(&DataKey::Signers, &signers);
            env.storage()
                .instance()
                .set(&DataKey::Threshold, &threshold);
        }
    }

    #[contractimpl]
    impl CustomAccountInterface for MultisigAccount {
        type Signature = Vec<(u32, BytesN<64>)>;
        type Error = Error;

        fn __check_auth(
            env: Env,
            signature_payload: Hash<32>,
            signatures: Vec<(u32, BytesN<64>)>,
            _auth_contexts: Vec<Context>,
        ) -> Result<(), Error> {
            let threshold: u32 = env.storage().instance().get(&DataKey::Threshold).unwrap();
            if signatures.len() < threshold {
                return Err(Error::NotEnoughSignatures);
            }
            let signers: Vec<BytesN<32>> = env.storage().instance().get(&DataKey::Signers).unwrap();
            let message: soroban_sdk::Bytes = signature_payload.into();

            let mut last_index: i64 = -1;
            for (index, sig) in signatures.iter() {
                if i64::from(index) <= last_index {
                    return Err(Error::SignaturesOutOfOrder);
                }
                last_index = i64::from(index);
                let public_key = signers.get(index).unwrap();
                env.crypto().ed25519_verify(&public_key, &message, &sig);
            }
            Ok(())
        }
    }
}

/// Builds and signs a `SorobanAuthorizationEntry` for `account` (a deployed
/// `multisig::MultisigAccount`) authorizing a single top-level call —
/// `contract.fn_name(args)` — with no sub-invocations.
fn sign_multisig_auth(
    env: &Env,
    account: &Address,
    contract: &Address,
    fn_name: &str,
    args: soroban_sdk::Vec<soroban_sdk::Val>,
    signing_keys: &[(u32, &ed25519_dalek::SigningKey)],
    nonce: i64,
) -> soroban_sdk::xdr::SorobanAuthorizationEntry {
    use ed25519_dalek::Signer as _;
    use soroban_sdk::xdr::{
        self, HashIdPreimage, HashIdPreimageSorobanAuthorization, InvokeContractArgs, Limited,
        Limits, ScAddress, SorobanAddressCredentials, SorobanAuthorizationEntry,
        SorobanAuthorizedFunction, SorobanAuthorizedInvocation, SorobanCredentials, WriteXdr,
    };
    use soroban_sdk::TryFromVal;

    let network_id = xdr::Hash([0u8; 32]); // the test env's default.
    let signature_expiration_ledger = env.ledger().sequence() + 100;
    let address: ScAddress = account.into();
    let invocation = SorobanAuthorizedInvocation {
        function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
            contract_address: contract.into(),
            function_name: fn_name.try_into().unwrap(),
            args: args.into(),
        }),
        sub_invocations: Default::default(),
    };
    // `SorobanCredentials::Address` hashes the preimage *without* the
    // address folded in; see the long comment at this same spot in
    // contracts/escrow/src/test.rs for how that was confirmed.
    let preimage = HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
        network_id,
        nonce,
        signature_expiration_ledger,
        invocation: invocation.clone(),
    });
    let mut buf = std::vec::Vec::new();
    preimage
        .write_xdr(&mut Limited::new(&mut buf, Limits::none()))
        .unwrap();
    let payload: [u8; 32] = env
        .crypto()
        .sha256(&Bytes::from_slice(env, &buf))
        .to_array();

    let sigs: soroban_sdk::Vec<(u32, BytesN<64>)> = soroban_sdk::vec![env];
    let mut sigs = sigs;
    for (index, key) in signing_keys {
        let sig = key.sign(&payload);
        sigs.push_back((*index, BytesN::from_array(env, &sig.to_bytes())));
    }
    let signature = xdr::ScVal::try_from_val(env, &sigs.to_val()).unwrap();

    SorobanAuthorizationEntry {
        root_invocation: invocation,
        credentials: SorobanCredentials::Address(SorobanAddressCredentials {
            address,
            nonce,
            signature_expiration_ledger,
            signature,
        }),
    }
}

#[test]
fn admin_as_a_multisig_signs_config_changes_and_admin_transfer() {
    let env = Env::default();
    env.ledger().set_timestamp(START);

    let keys: std::vec::Vec<ed25519_dalek::SigningKey> = (0..3)
        .map(|_| ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng))
        .collect();
    let signer_bytes: soroban_sdk::Vec<BytesN<32>> = soroban_sdk::vec![
        &env,
        BytesN::from_array(&env, &keys[0].verifying_key().to_bytes()),
        BytesN::from_array(&env, &keys[1].verifying_key().to_bytes()),
        BytesN::from_array(&env, &keys[2].verifying_key().to_bytes()),
    ];
    let admin = env.register(multisig::MultisigAccount, (signer_bytes, 2u32));

    let arbitrator = Address::generate(&env);
    let fee_recipient = Address::generate(&env);
    let escrow_wasm_hash = env.deployer().upload_contract_wasm(escrow_wasm::WASM);
    let factory_id = env.register(
        Factory,
        (Config {
            admin: admin.clone(),
            escrow_wasm_hash,
            arbitrator,
            fee_recipient,
            fee_bps: FEE_BPS,
        },),
    );
    let factory = FactoryClient::new(&env, &factory_id);

    // set_config: one signature isn't enough, two genuinely is — the real
    // __check_auth ran both times, not a mock standing in for it.
    let mut new_config = factory.config();
    new_config.fee_bps = 300;
    let args = soroban_sdk::vec![&env, new_config.clone().into_val(&env)];

    let one = sign_multisig_auth(
        &env,
        &admin,
        &factory_id,
        "set_config",
        args.clone(),
        &[(0, &keys[0])],
        0,
    );
    env.set_auths(&[one]);
    assert!(factory.try_set_config(&new_config).is_err());
    assert_eq!(factory.config().fee_bps, FEE_BPS);

    let two = sign_multisig_auth(
        &env,
        &admin,
        &factory_id,
        "set_config",
        args,
        &[(0, &keys[0]), (1, &keys[1])],
        1,
    );
    env.set_auths(&[two]);
    factory.set_config(&new_config);
    assert_eq!(factory.config().fee_bps, 300);

    // allow_token, signed by a different pair of signers.
    let token = Address::generate(&env);
    let limits = Some(TokenLimits {
        min_amount: 1,
        max_amount: i128::MAX,
    });
    let args = soroban_sdk::vec![&env, token.into_val(&env), limits.into_val(&env)];
    let sig = sign_multisig_auth(
        &env,
        &admin,
        &factory_id,
        "allow_token",
        args,
        &[(1, &keys[1]), (2, &keys[2])],
        2,
    );
    env.set_auths(&[sig]);
    factory.allow_token(&token, &limits);

    // propose_admin, signed by the multisig; accept_admin, signed normally
    // by the new (plain) admin — the transfer away from a multisig works.
    let new_admin = Address::generate(&env);
    let args = soroban_sdk::vec![&env, new_admin.clone().into_val(&env)];
    let sig = sign_multisig_auth(
        &env,
        &admin,
        &factory_id,
        "propose_admin",
        args,
        &[(0, &keys[0]), (2, &keys[2])],
        3,
    );
    env.set_auths(&[sig]);
    factory.propose_admin(&new_admin);
    assert_eq!(factory.pending_admin(), Some(new_admin.clone()));

    env.mock_all_auths();
    factory.accept_admin();
    assert_eq!(factory.config().admin, new_admin);
}
