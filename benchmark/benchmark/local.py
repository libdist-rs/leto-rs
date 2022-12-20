# Copyright(C) Facebook, Inc. and its affiliates.
import subprocess
from math import ceil
from os.path import basename, splitext
from time import sleep

from benchmark.commands import CommandMaker
from benchmark.config import LocalCommittee, NodeParameters, BenchParameters, ConfigError
from benchmark.logs import LogParser, ParseError
from benchmark.utils import Print, BenchError, PathMaker


class LocalBench:
    BASE_PORT = 3000

    def __init__(self, node_parameters_dict, bench_parameters_dict):
        try:
            self.bench_parameters = BenchParameters(bench_parameters_dict)
            self.node_parameters = NodeParameters(node_parameters_dict)
        except ConfigError as e:
            raise BenchError('Invalid nodes or bench parameters', e)

    def __getattr__(self, attr):
        return getattr(self.bench_parameters, attr)

    def _background_run(self, command, log_file):
        name = splitext(basename(log_file))[0]
        cmd = f'{command} &> {log_file}'
        subprocess.run(['tmux', 'new', '-d', '-s', name, cmd], check=True)

    def _kill_nodes(self):
        try:
            cmd = CommandMaker.kill().split()
            subprocess.run(cmd, stderr=subprocess.DEVNULL)
        except subprocess.SubprocessError as e:
            raise BenchError('Failed to kill testbed', e)

    def run(self, debug=False):
        assert isinstance(debug, bool)
        Print.heading('Starting local benchmark')

        # Kill any previous testbed.
        self._kill_nodes()

        try:
            Print.info('Setting up testbed...')
            nodes, rate = self.node_parameters.servers, self.rate[0]
            clients = nodes

            # Cleanup all files.
            cmd = f'{CommandMaker.clean_logs()} ; {CommandMaker.cleanup()}'
            subprocess.run([cmd], shell=True, stderr=subprocess.DEVNULL)
            sleep(0.5)  # Removing the store may take time.

            # Recompile the latest code.
            cmd = CommandMaker.compile()
            subprocess.run(
                [cmd], shell=True, check=True, cwd=PathMaker.node_crate_path()
            )

            # Create alias for the client and nodes binary.
            cmd = CommandMaker.alias_binaries(PathMaker.binary_path())
            subprocess.run([cmd], shell=True)

            # Generate keys
            key_files = [PathMaker.key_file(i) for i in range(nodes)]
            cmd = CommandMaker.generate_keys(nodes, ".").split()
            subprocess.run(cmd, check=True)
            # keys = [Key.from_file(filename) for filename in key_files]

            # Generate server config
            committee = LocalCommittee(nodes, self.BASE_PORT)
            cmd = CommandMaker.generate_server_config(
                self.node_parameters, 
                self.bench_parameters
            ).split()
            subprocess.run(cmd, check=True)

            # Run the primaries (except the faulty ones).
            assert 3*self.bench_parameters.faults < self.node_parameters.servers
            to_launch = self.node_parameters.servers - self.bench_parameters.faults
            for i in range(to_launch):
                cmd = CommandMaker.run_server(
                    PathMaker.key_file(i),
                    i,
                    PathMaker.server_config_file(),
                    debug=debug
                )
                log_file = PathMaker.server_log_file(i)
                self._background_run(cmd, log_file)

            # Run the clients (they will wait for the nodes to be ready).
            for i in range(to_launch):
                cmd = CommandMaker.run_client(
                    nodes + i,
                    PathMaker.client_config_file(),
                )
                log_file = PathMaker.client_log_file(i)
                self._background_run(cmd, log_file)

            # Wait for all transactions to be processed.
            Print.info(f'Running benchmark ({self.duration} sec)...')
            sleep(self.duration)
            self._kill_nodes()

            # Parse logs and return the parser.
            Print.info('Parsing logs...')
            # return LogParser.process(PathMaker.logs_path(), faults=self.faults)

        except (subprocess.SubprocessError, ParseError) as e:
            self._kill_nodes()
            raise BenchError('Failed to run benchmark', e)
