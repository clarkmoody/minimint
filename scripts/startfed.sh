#!/usr/bin/env bash

SIZE="$1"

cargo build --release --all-targets

bitcoind -regtest -fallbackfee=0.0004 -txindex -server -rpcuser=bitcoin -rpcpassword=bitcoin &

echo "Waiting for bitcoind to start"
sleep 3

for ((ID=$2; ID<SIZE; ID++)); do
  echo "starting mint $ID"
  (target/release/server cfg/server-$ID.json 2>&1 | sed -e "s/^/mint $ID: /" ) &
done

read -p "Press enter to stop processes"

kill 0