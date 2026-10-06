# TrustEscrow contracts

Soroban contracts for TrustEscrow, a peer-to-peer escrow for online trade between strangers.

A buyer deposits, the seller proves delivery on-chain, the buyer proves receipt (by handing over a delivery code or signing a confirmation), and only then is the seller paid. No timeout ever pays the seller: a silent buyer escalates to an arbitrator, and an arbitrator who doesn't rule in time refunds the buyer.

## Layout

| Path | Crate | What it is |
|---|---|---|
| [crates/types](crates/types) | `trustescrow-types` | Types shared by both contracts |
| [crates/code](crates/code) | `trustescrow-code` | Reference implementation of the delivery code: encoding, normalisation, hash |
| [contracts/escrow](contracts/escrow) | `trustescrow-escrow` | One instance per trade: custody, lifecycle, payout |
| [contracts/factory](contracts/factory) | `trustescrow-factory` | Deploys escrows; token allowlist, operator config for new escrows, two-step admin transfer |
| [test-vectors](test-vectors) | — | Delivery code vectors every client must pass |
| [scripts](scripts) | — | Testnet deployment |

## Build and test

`rust-toolchain.toml` pins an exact Rust version, not `stable`: the same source compiles to a different WASM on every new Rust release, and clients pin the escrow WASM hash, so the toolchain that produces it has to be reproducible too. `rustup` installs the pinned version and the `wasm32v1-none` target automatically on first use.

```sh
make build   # release WASM into target/wasm32v1-none/release/
make test    # builds the WASM first — the factory tests deploy the real escrow binary
make clippy
```

A release's WASM is exactly `make build`'s output on the tagged commit, with the toolchain `rust-toolchain.toml` pins at that tag — that's the whole build command; nothing else feeds into the hash. CI runs it on every push and prints both WASMs' sha256 so that number is independently checkable, not just asserted in release notes.

`cargo test` on its own fails to compile the factory tests if the escrow WASM hasn't been built yet. Dependencies are compiled with optimisation even in test builds, because the Soroban host is very slow without it; the first build takes a while, later ones are quick.

Besides unit tests for every transition and deadline boundary, the escrow has a seeded randomised state-machine test that drives escrows through random call sequences and checks conservation, terminality and the two-sided release rule after every step. CI runs formatting, the WASM build, clippy and all tests on every push.

## Lifecycle at a glance

```
Created ──fund──▶ Funded ──submit_proof──▶ Delivered ──release_with_code / confirm──▶ Released
   │                │  └─submit_proof_with_code (in person)─────────────────────────▶ Released
   │                │                         │
   ▼                ▼                         ▼ dispute / escalate (after receipt_deadline)
Cancelled        Refunded ◀── delivery      Disputed ──resolve──▶ Released | Refunded
                 timeout / seller_refund      └── arbitration deadline ──▶ Refunded
```

Either deadline can be pushed later without changing state: the buyer calls `extend_delivery` while `Funded`, the seller calls `extend_receipt` while `Delivered`. Each only ever benefits the other side — a seller who's running late gets more time to deliver because the buyer chose to wait, and a buyer who needs more time to check the goods gets it because the seller chose to wait — and neither can push a deadline earlier or past the one-year window cap.

## Delivery codes

A delivery code is 80 bits of entropy written as 16 Crockford base32 characters and shown as `K7M2-9XQF-4TBN-R3WD`. The escrow stores `sha256` of the 16 canonical characters. Because that hash is public, the code's length is its only defence against brute force: never generate shorter codes.

Clients must normalise input before hashing or submitting it: strip whitespace and hyphens, uppercase, and map `I`/`L` to `1` and `O` to `0`. [crates/code](crates/code) implements this, and [test-vectors/delivery-codes.json](test-vectors/delivery-codes.json) holds vectors produced by an independent implementation. A client that passes them produces the same bytes the contract checks.

## Token requirements

The escrow is only ever as good as the settlement token's own behaviour, which the contract cannot see or control. `contracts/escrow/src/test.rs` exercises the real built-in Stellar Asset Contract, not a mock, to show what actually happens:

- **A party who can't hold the asset blocks only the transfer that would pay them, never the other side's exit.** If the seller is never authorised to receive it, release and confirm fail, but the buyer still gets refunded through a dispute. If the buyer is deauthorised after funding, every refund fails, but release still works as long as the seller delivers. Either way the escrow keeps moving — just not in every direction.
- **Clawback is the one flag that can leave an escrow with no exit at all.** The contract records `amount` once at funding and never re-reads the real token balance to check it; `get()` has no way to learn that the tokens it believes it holds are gone. If the issuer claws back from the escrow's own balance, every exit — release, every refund path, `seller_refund` — tries to move the full original amount and fails, forever. The escrow is left recording a state it can never leave.

**The factory admin must never allowlist a clawback-enabled asset.** There is no on-chain way for the contract to defend against its own balance being taken out from under it; refusing the asset at the allowlist is the only mitigation. `AUTH_REQUIRED` and `AUTH_REVOCABLE` assets are safe to allowlist — they can block a specific party's payout, but never both sides' exits at once, and never desynchronise the record from reality the way clawback can.

## Deploying (testnet)

With the [Stellar CLI](https://developers.stellar.org/docs/tools/cli) and a funded identity:

```sh
SOURCE=admin ARBITRATOR=G... FEE_RECIPIENT=G... TOKEN=C... \
  MIN_AMOUNT=1 MAX_AMOUNT=10000000000 scripts/deploy-testnet.sh
```

The script uploads the escrow WASM, deploys the factory, allowlists the settlement token between `MIN_AMOUNT` and `MAX_AMOUNT` (the token's smallest unit) and writes the factory id and escrow WASM hash to `deployments/testnet.env`. These bounds are required, not defaulted: while the contracts are unaudited, how much value one escrow can hold is a deliberate choice, not a quiet default.

Clients must pin the escrow WASM hash they have audited and refuse to fund an escrow instance running anything else. A factory config change only affects escrows created after it, so a swapped WASM hash can never reach an open trade.

Pinning the WASM hash is not enough on its own: anyone can deploy that same audited WASM directly, outside the factory, with their own arbitrator and fee recipient. Clients must also check **factory provenance** before funding: each escrow stores the `salt` the buyer passed to `Factory::create`, and `factory.escrow_address(escrow.buyer, escrow.salt)` must equal the escrow's own address. Only the factory's deployer address can produce that match, so a directly-deployed escrow fails this check no matter what salt it claims.

Handing the factory to a new admin takes two steps: the current admin calls `propose_admin`, and nothing changes until the proposed address calls `accept_admin`. `set_config` cannot change the admin, so a mistyped address can never lock the factory. A pending proposal can be withdrawn with `cancel_admin_transfer`, which emits `AdminTransferCancelled` so a withdrawn proposal is visible to anyone watching events, the same as a completed one.


## Live on testnet

| What | Id |
|---|---|
| Factory | `CDMCI4VW5XARBPITNHNENEDVKBMKDICFCKJJ3RYDAKDBZRQMGPUV5JIO` |
| Escrow WASM hash | `7a91c255c29edb7114a546026e807f144a8adc58460e4608e314a77ac629281d` |
| Settlement token (test asset SAC) | `CBXMP6YK4B4WZKN4UAF7OZUEGFEUURVPSUQS5QGG5SG5DRWBRQDWAOOL` |

`deployments/testnet.env` holds the same values plus the admin, arbitrator and fee recipient.

This deployment predates the toolchain pin above, built with whatever was `stable` at the time; rebuilding it with a pinned compiler was attempted but did not reproduce the recorded hash, and the exact version originally used wasn't recoverable. The next testnet deploy will be built with the pinned toolchain, so its hash can be reproduced from here on.

One escrow has been run end to end against this deployment: [create](https://stellar.expert/explorer/testnet/tx/16515feb9764f9ef3021bdbd20744c01d467ce6eee22a8cb31d038b964589e7d), [fund](https://stellar.expert/explorer/testnet/tx/8e305e4323098a99564e985e3ca671e805219ae66250d1e3b17d42a5e15c1c02), [submit_proof](https://stellar.expert/explorer/testnet/tx/48c038bed1a5c6d8428339902cd74d54077cc7c494a51ba3499658db7e1b875a), then release with the buyer's delivery code. Of 100 units deposited the seller received 98.5 and the fee recipient 1.5, and the escrow ended `Released` via `Code` holding nothing.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
