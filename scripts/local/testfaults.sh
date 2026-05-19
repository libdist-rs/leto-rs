#!/bin/bash

# Killall previous instances
killall node

N=${N:-4}

echo "Building..."
cargo b --all

echo "Clearing the database"
rm -rf db-*.db

echo "Starting $(($N-1)) servers"
for((i=0;i<$((N-1));i++)); do
    # Start the server
    timeout 60 cargo r -p node \
        -- \
        -vvvv \
        server \
        --id "${i}" \
        --key-file examples/keys-${i}.json &> test-log${i}.log &
done

sleep 1 
echo "Starting the client" 
timeout 55 cargo r -p node \
    -- \
    -v \
    client \
    --id 4 &> test-log-client.log