#!/bin/bash

cargo b --all

echo "Starting 4 servers"
cargo r -p node -- server --id 0 &> test-log0.log &
cargo r -p node -- server --id 1 &> test-log1.log &
cargo r -p node -- server --id 2 &> test-log2.log &
cargo r -p node -- server --id 3 &> test-log3.log &
sleep 1 
echo "Starting the client" 
cargo r -p node -- client --id 4