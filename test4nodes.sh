#!/bin/bash

cargo b --all

echo "Starting 4 servers"
cargo r -p node -- server --id 0 &
cargo r -p node -- server --id 1 &
cargo r -p node -- server --id 2 &
cargo r -p node -- server --id 3 &
sleep 1 
echo "Starting the client" 
cargo r -p node -- client --id 4