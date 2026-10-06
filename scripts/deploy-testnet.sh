#!/usr/bin/env bash
# Deploy the TrustEscrow factory to Stellar testnet.
#
# Uploads the escrow WASM, deploys the factory pointing at that hash,
# allowlists the settlement token, and writes the results to
# deployments/testnet.env.
#
# Requires the Stellar CLI (https://developers.stellar.org/docs/tools/cli) and
# a funded testnet identity.
#
# Usage:
#   SOURCE=admin ARBITRATOR=G... FEE_RECIPIENT=G... TOKEN=C... \
#     MIN_AMOUNT=1 MAX_AMOUNT=10000000000 scripts/deploy-testnet.sh
#
# FEE_BPS defaults to 150 (1.5%). The contract caps it at 1000.
#
# MIN_AMOUNT and MAX_AMOUNT are required, not defaulted: the contracts are
# unaudited, so how much value one escrow can hold is a deliberate choice for
# whoever is deploying, not something this script should pick quietly. Both
# are in the token's smallest unit (e.g. for a 7-decimal asset, 10000000000
# is 1,000 units).

set -euo pipefail

: "${SOURCE:?set SOURCE to a stellar CLI identity or secret key}"
: "${ARBITRATOR:?set ARBITRATOR to the arbitrator address}"
: "${FEE_RECIPIENT:?set FEE_RECIPIENT to the fee recipient address}"
: "${TOKEN:?set TOKEN to the settlement token contract id (e.g. testnet USDC SAC)}"
: "${MIN_AMOUNT:?set MIN_AMOUNT, the smallest order amount this token will accept}"
: "${MAX_AMOUNT:?set MAX_AMOUNT, the largest order amount this token will accept}"
FEE_BPS="${FEE_BPS:-150}"
NETWORK="${NETWORK:-testnet}"

cd "$(dirname "$0")/.."
command -v stellar >/dev/null || { echo "stellar CLI not found" >&2; exit 1; }

make build

WASM_DIR=target/wasm32v1-none/release
ADMIN=$(stellar keys address "$SOURCE" 2>/dev/null || echo "$SOURCE")

echo "Uploading escrow WASM..."
ESCROW_WASM_HASH=$(stellar contract upload \
  --wasm "$WASM_DIR/trustescrow_escrow.wasm" \
  --source "$SOURCE" --network "$NETWORK")

echo "Deploying factory..."
FACTORY_ID=$(stellar contract deploy \
  --wasm "$WASM_DIR/trustescrow_factory.wasm" \
  --source "$SOURCE" --network "$NETWORK" \
  -- \
  --config "{\"admin\":\"$ADMIN\",\"escrow_wasm_hash\":\"$ESCROW_WASM_HASH\",\"arbitrator\":\"$ARBITRATOR\",\"fee_recipient\":\"$FEE_RECIPIENT\",\"fee_bps\":$FEE_BPS}")

echo "Allowlisting settlement token..."
# `limits` is Option<TokenLimits>; a JSON object means Some, as with
# `--config` above. Not verified against a live network in the PR that added
# it (#5) — no local Stellar node was available to confirm the CLI's exact
# flag generation for an optional struct argument, so if this errors, check
# `stellar contract invoke --id "$FACTORY_ID" --network "$NETWORK" --source
# "$SOURCE" -- allow_token --help` and adjust.
stellar contract invoke --id "$FACTORY_ID" \
  --source "$SOURCE" --network "$NETWORK" \
  -- allow_token --token "$TOKEN" \
  --limits "{\"min_amount\":\"$MIN_AMOUNT\",\"max_amount\":\"$MAX_AMOUNT\"}"

mkdir -p deployments
cat > "deployments/$NETWORK.env" <<EOF
NETWORK=$NETWORK
FACTORY_ID=$FACTORY_ID
ESCROW_WASM_HASH=$ESCROW_WASM_HASH
ADMIN=$ADMIN
ARBITRATOR=$ARBITRATOR
FEE_RECIPIENT=$FEE_RECIPIENT
FEE_BPS=$FEE_BPS
TOKEN=$TOKEN
MIN_AMOUNT=$MIN_AMOUNT
MAX_AMOUNT=$MAX_AMOUNT
EOF

echo
echo "Factory:          $FACTORY_ID"
echo "Escrow WASM hash: $ESCROW_WASM_HASH"
echo "Written to deployments/$NETWORK.env. Pin the WASM hash in the SDK."
