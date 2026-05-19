#!/bin/bash

# Killall previous instances
killall node

N=${N:-4}

echo "Building..."
cargo b --all --features=microbench

echo "Clearing the database"
rm -rf db-*.db

echo "Starting ${N} servers"
for((i=0;i<$N;i++)); do
    # Start the server
    cargo r -p node \
        --features=consensus/microbench \
        -- \
        -vvvv \
        server \
        --id "${i}" \
        --key-file examples/keys-${i}.json &> test-log${i}.log &
done

sleep 1 
echo "Starting the client" 
cargo r -p node \
    --features=consensus/microbench \
    -- \
    -vvv \
    client \
    --id 4 &> test-log-client.log