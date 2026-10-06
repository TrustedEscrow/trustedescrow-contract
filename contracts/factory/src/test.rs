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
    Bytes, BytesN, Env, Event, String,
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

// --- Storage TTL ---------------------------------------------------------------

#[test]
fn admin_actions_restore_the_full_instance_ttl() {
    // Hardcoded independently of TTL_EXTEND_TO, so this actually fails if
    // that constant's arithmetic is ever wrong — asserting against the
    // constant itself would trivially agree with whatever it computes to.
    const EXPECTED_EXTEND_TO: u32 = 120 * 17_280;

    let s = setup();
    let full = s.ttl();
    let max_ttl = s
        .env
        .as_contract(&s.factory.address, || s.env.storage().max_ttl());
    assert_eq!(full, EXPECTED_EXTEND_TO.min(max_ttl));

    s.advance_ledgers(full - 10);
    assert_eq!(s.ttl(), 10);
    // allow_token only touches persistent storage (the token entry); use a
    // call that actually touches the Config instance entry.
    s.factory.set_config(&s.factory.config());
    assert_eq!(s.ttl(), full);
}

#[test]
fn the_ttl_threshold_itself_is_thirty_days_not_just_nonzero() {
    // Letting the TTL run down to 10 remaining (as the test above does)
    // can't distinguish the real threshold (30 days of ledgers) from a
    // much smaller wrong one — both are comfortably above 10, so
    // extend_ttl's "below threshold" condition is true either way. This
    // stops at a remaining TTL that only the *real* threshold is above.
    const DAY_IN_LEDGERS: u32 = 17_280;
    const EXPECTED_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
    const REMAINING: u32 = EXPECTED_THRESHOLD - DAY_IN_LEDGERS; // still below the real threshold

    let s = setup();
    let full = s.ttl();
    s.advance_ledgers(full - REMAINING);
    assert_eq!(s.ttl(), REMAINING);
    s.factory.set_config(&s.factory.config());
    assert_eq!(s.ttl(), full);
}

#[test]
fn allow_token_restores_the_full_persistent_ttl_of_the_token_entry() {
    const EXPECTED_EXTEND_TO: u32 = 120 * 17_280;

    let s = setup();
    let full = s.token_ttl(&s.token);
    let max_ttl = s
        .env
        .as_contract(&s.factory.address, || s.env.storage().max_ttl());
    assert_eq!(full, EXPECTED_EXTEND_TO.min(max_ttl));

    s.advance_ledgers(full - 10);
    assert_eq!(s.token_ttl(&s.token), 10);
    s.factory.allow_token(&s.token, &s.wide_limits());
    assert_eq!(s.token_ttl(&s.token), full);
}

// --- Boundaries ------------------------------------------------------------------

#[test]
fn limits_with_min_equal_to_max_are_valid() {
    // A token restricted to exactly one amount is a legitimate limit, not
    // an error — only min > max should be rejected.
    let s = setup();
    s.factory.allow_token(
        &s.token,
        &Some(TokenLimits {
            min_amount: 100,
            max_amount: 100,
        }),
    );
    let mut order = s.order(&Address::generate(&s.env));
    order.amount = 100;
    s.factory.create(&order, &s.salt(1));
}

#[test]
fn fee_at_the_cap_is_accepted() {
    // fee_above_cap_is_rejected only exercises MAX_FEE_BPS + 1; the cap
    // itself must still be a valid, inclusive boundary.
    let s = setup();
    let mut config = s.factory.config();
    config.fee_bps = MAX_FEE_BPS;
    s.factory.set_config(&config);
    assert_eq!(s.factory.config().fee_bps, MAX_FEE_BPS);
}
