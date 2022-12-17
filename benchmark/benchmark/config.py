# Copyright(C) Facebook, Inc. and its affiliates.
from json import dump, load
from collections import OrderedDict


class ConfigError(Exception):
    pass


class Key:
    def __init__(self, alg, secret_bytes, system):
        self.alg = alg
        self.secret_bytes = secret_bytes
        self.system = system

    @classmethod
    def from_file(cls, filename):
        assert isinstance(filename, str)
        with open(filename, 'r') as f:
            data = load(f)
        return cls(data['alg'], data['secret_bytes'], data['system'])


class Committee:

    def __init__(self, addresses, consensus_base_port, mempool_base_port, client_base_port):
        assert isinstance(addresses, OrderedDict)
        assert all(isinstance(x, int) for x in addresses.keys())
        assert all(
            isinstance(x, str) for x in addresses.values()
        )
        assert isinstance(consensus_base_port, int) and consensus_base_port > 1024       
        assert isinstance(mempool_base_port, int) and mempool_base_port > 1024
        assert isinstance(client_base_port, int) and client_base_port > 1024

        consensus_port = consensus_base_port
        mempool_port = mempool_base_port
        client_port = client_base_port
        self.node_names = OrderedDict()
        for name, host in addresses.items():
            primary_addr = {
                'consensus': f'{host}:{consensus_port}',
                'mempool': f'{host}:{mempool_port}',
                'client': f'{host}:{client_port}'
            }
            consensus_port += 1
            mempool_port += 1
            client_port += 1
            self.node_names[name] = primary_addr

    def primary_addresses(self, faults=0):
        ''' Returns an ordered list of primaries' addresses. '''
        assert faults < self.size()
        addresses = []
        good_nodes = self.size() - faults
        for name in list(self.node_names.keys())[:good_nodes]:
            addresses += [self.node_names[name]]
        return addresses

    def ips(self, name=None):
        ''' Returns all the ips associated with an authority (in any order). '''
        if name is None:
            names = list(self.node_names.keys())
        else:
            names = [name]

        ips = set()
        for name in names:
            addresses = self.node_names[name]
            ips.add(self.ip(addresses['consensus']))
            ips.add(self.ip(addresses['mempool']))
            ips.add(self.ip(addresses['client']))

        return list(ips)

    def remove_nodes(self, nodes):
        ''' remove the `nodes` last nodes from the committee. '''
        assert nodes < self.size()
        for _ in range(nodes):
            self.node_names.popitem()

    def size(self):
        ''' Returns the number of authorities. '''
        return len(self.node_names)

    @staticmethod
    def ip(address):
        assert isinstance(address, str)
        return address.split(':')[0]


class LocalCommittee(Committee):
    def __init__(self, nodes, consensus_base_port = 7001, mempool_base_port = 8001, client_base_port = 9001):
        assert isinstance(nodes, int)
        assert nodes > 3
        assert isinstance(consensus_base_port, int)
        assert isinstance(mempool_base_port, int)
        assert isinstance(client_base_port, int)
        addresses = OrderedDict()
        for x in range(nodes):
            addresses[x] = '127.0.0.1'
        super().__init__(
            addresses, 
            consensus_base_port=consensus_base_port, 
            mempool_base_port=mempool_base_port, 
            client_base_port=client_base_port
        )


class NodeParameters:
    def __init__(self, json):
        inputs = []
        try:
            inputs += [json['servers']]

            if 'network_delay' not in json:
                json['network_delay'] = 500
            inputs += [json['network_delay']]

            if 'gc_depth' not in json:
                json['gc_depth'] = 50
            inputs += [json['gc_depth']]

            if 'sync_retry_nodes' not in json:
                json['sync_retry_nodes'] = 3
            inputs += [json['sync_retry_nodes']]

            if 'sync_retry_delay' not in json:
                json['sync_retry_delay'] = 10_000
            inputs += [json['sync_retry_delay']]

            inputs += [json['batch_size']]
            inputs += [json['max_batch_delay']]
            inputs += [json['tx_size']]

        except KeyError as e:
            raise ConfigError(f'Malformed parameters: missing key {e}')

        if not all(isinstance(x, int) for x in inputs):
            raise ConfigError('Invalid parameters type')

        self.servers = json['servers']
        self.network_delay = json['network_delay']
        self.gc_depth = json['gc_depth']
        self.sync_retry_nodes = json['sync_retry_nodes']
        self.sync_retry_delay = json['sync_retry_delay']
        self.batch_size = json['batch_size']
        self.max_batch_size = json['max_batch_delay']
        self.tx_size = json['tx_size']
        self.burst_interval = 50

class BenchParameters:
    def __init__(self, json):
        try:
            self.faults = int(json['faults'])

            rate = json['rate']
            rate = rate if isinstance(rate, list) else [rate]
            if not rate:
                raise ConfigError('Missing input rate')
            self.rate = [int(x) for x in rate]

            local = json['local']
            if not local:
                local = True
            assert isinstance(local, bool)
            self.local = local

            self.duration = int(json['duration'])

            self.runs = int(json['runs']) if 'runs' in json else 1

        except KeyError as e:
            raise ConfigError(f'Malformed bench parameters: missing key {e}')

        except ValueError:
            raise ConfigError('Invalid parameters type')


class PlotParameters:
    def __init__(self, json):
        try:
            faults = json['faults']
            faults = faults if isinstance(faults, list) else [faults]
            self.faults = [int(x) for x in faults] if faults else [0]

            nodes = json['nodes']
            nodes = nodes if isinstance(nodes, list) else [nodes]
            if not nodes:
                raise ConfigError('Missing number of nodes')
            self.nodes = [int(x) for x in nodes]

            self.tx_size = int(json['tx_size'])

            max_lat = json['max_latency']
            max_lat = max_lat if isinstance(max_lat, list) else [max_lat]
            if not max_lat:
                raise ConfigError('Missing max latency')
            self.max_latency = [int(x) for x in max_lat]

        except KeyError as e:
            raise ConfigError(f'Malformed bench parameters: missing key {e}')

        except ValueError:
            raise ConfigError('Invalid parameters type')

        if len(self.nodes) > 1 and len(self.workers) > 1:
            raise ConfigError(
                'Either the "nodes" or the "workers can be a list (not both)'
            )
