# Copyright(C) Facebook, Inc. and its affiliates.
from math import ceil
from os.path import join

from benchmark.config import NodeParameters, BenchParameters
from benchmark.utils import PathMaker


class CommandMaker:

    @staticmethod
    def cleanup():
        return (
            f'rm -r .db-* ; rm *.json ; mkdir -p {PathMaker.results_path()}'
        )

    @staticmethod
    def clean_logs():
        return f'rm -r {PathMaker.logs_path()} ; mkdir -p {PathMaker.logs_path()}'

    @staticmethod
    def compile():
        return f'cargo build --quiet --release -p node --features=benchmark'

    @staticmethod
    def generate_keys(num_nodes, directory):
        assert isinstance(directory, str)
        assert isinstance(num_nodes, int) 
        assert num_nodes > 3
        return f'./node keys --num-servers {num_nodes} -o {directory}'

    @staticmethod
    def run_server(key_file, id, server_file, debug=False):
        assert isinstance(key_file, str)
        assert isinstance(server_file, str)
        assert isinstance(debug, bool)
        v = '-vvvv' if debug else '-vvv'
        return f'./node {v} server --id {id} --key-file {key_file} --config ./{server_file}'

    @staticmethod
    def run_client(id, client_file):
        assert isinstance(id, int)
        assert isinstance(client_file, str)
        return f'./node -vvv client --id {id} --config ./{client_file}'

    @staticmethod
    def kill():
        return 'tmux kill-server'

    @staticmethod
    def generate_server_config(node_params, bench_params):
        assert isinstance(node_params, NodeParameters)
        assert isinstance(bench_params, BenchParameters)

        # Root command
        cmd = f'./node config '

        # Parameters
        cmd += f'--servers {node_params.servers} '
        cmd += f'--network-delay {node_params.network_delay} '
        cmd += f'--gc-depth {node_params.gc_depth} '
        cmd += f'--sync-retry-nodes {node_params.sync_retry_nodes} '
        cmd += f'--sync-retry-delay {node_params.sync_retry_delay} '
        cmd += f'--consensus-port {3000} '
        cmd += f'--mempool-port {6000} '
        cmd += f'--client-port {9000} '
        cmd += f'--batch-size {node_params.batch_size} '
        cmd += f'--tx-size {node_params.tx_size} '
        cmd += f'--burst-interval {node_params.burst_interval} '

        # Setup IPs
        ips = ["127.0.0.1"] * node_params.servers
        for ip in ips:
            cmd += f'--ip {ip} '

        # Config local testing/remote testing
        if bench_params.local:
            cmd += f'--local '
        
        # Setup rate
        rate_per_client = ceil(bench_params.rate[0]/node_params.servers)
        num_bursts_per_second = ceil(1000/node_params.burst_interval)
        rate_per_burst = int(ceil(rate_per_client/num_bursts_per_second))
        cmd += f'--txs-per-burst {rate_per_burst} '

        # Output directory
        cmd += f'-o .'

        return cmd

    @staticmethod
    def alias_binaries(origin):
        assert isinstance(origin, str)
        node, client = join(origin, 'node'), join(origin, 'benchmark_client')
        return f'rm node ; rm benchmark_client ; ln -s {node} . ; ln -s {client} .'
