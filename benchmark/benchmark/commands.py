# Copyright(C) Facebook, Inc. and its affiliates.
from os.path import join

from benchmark.config import NodeParameters, BenchParameters, Committee
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
        return f'cargo build --quiet --release -p node'

    @staticmethod
    def generate_keys(num_nodes, directory):
        assert isinstance(directory, str)
        assert isinstance(num_nodes, int) 
        assert num_nodes > 3
        return f'./node keys --num-servers {num_nodes} -o {directory}'

    @staticmethod
    def run_primary(keys, committee, store, parameters, debug=False):
        assert isinstance(keys, str)
        assert isinstance(committee, str)
        assert isinstance(parameters, str)
        assert isinstance(debug, bool)
        v = '-vvv' if debug else '-vv'
        return (f'./node {v} run --keys {keys} --committee {committee} '
                f'--store {store} --parameters {parameters} primary')

    @staticmethod
    def run_client(address, size, rate, nodes):
        assert isinstance(address, str)
        assert isinstance(size, int) and size > 0
        assert isinstance(rate, int) and rate >= 0
        assert isinstance(nodes, list)
        assert all(isinstance(x, str) for x in nodes)
        nodes = f'--nodes {" ".join(nodes)}' if nodes else ''
        return f'./benchmark_client {address} --size {size} --rate {rate} {nodes}'

    @staticmethod
    def kill():
        return 'tmux kill-server'

    @staticmethod
    def generate_server_config(committee, node_params, bench_params):
        assert isinstance(committee, Committee)
        assert isinstance(node_params, NodeParameters)
        assert isinstance(bench_params, BenchParameters)
        cmd = f'./node config '
        cmd += f'--servers {node_params.json["servers"]} '
        if 'network_delay' in node_params.json:
            cmd += f'--network_delay {node_params.json["network_delay"]} '
        if 'gc_depth' in node_params.json:
            cmd += f'--gc_depth {node_params.json["gc_depth"]} '
        if 'sync_retry_nodes' in node_params.json:
            cmd += f'--sync_retry_nodes {node_params.json["sync_retry_nodes"]} '
        if 'sync_retry_delay' in node_params.json:
            cmd += f'--sync_retry_delay {node_params.json["sync_retry_delay"]} '
        # TODO: Generate ips/ip_file
        cmd += f'--local {bench_params.local} '
        # TODO: Generate consensus_port
        # TODO: Generate mempool_port
        # TODO: Generate client_port
        # TODO: Generate tx_port for the server
        cmd += f'--batch_size {node_params.json["batch_size"]} '
        cmd += f'--tx_size {node_params.json["tx_size"]} '
        # TODO: set burst_interval_ms
        # TODO: set txs per burst

        print(cmd)

    @staticmethod
    def alias_binaries(origin):
        assert isinstance(origin, str)
        node, client = join(origin, 'node'), join(origin, 'benchmark_client')
        return f'rm node ; rm benchmark_client ; ln -s {node} . ; ln -s {client} .'
