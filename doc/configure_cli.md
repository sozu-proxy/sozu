# Configuring sozu via command line

The sozu executable can be used to start the proxy, and configure it: adding new backend servers, reading metrics, etc.
It talks to the currently running proxy through a unix socket.

You can specify its path by adding to your `config.toml`:

```toml
command_socket = "path/to/your/command_folder/sock"
```

## Add a cluster with an http and https frontends

First you need to create a new cluster with an id and a load balancing policy (roundrobin or random):

```bash
sozu --config /etc/sozu/config.toml cluster add --id <my_cluster_id> --load-balancing-policy roundrobin
```

It won't show anything but you can verify that the cluster has been added successfully by querying sozu:

```bash
sozu --config /etc/sozu/config.toml query clusters
```

Then you need to add a backend:

```bash
sozu --config /etc/sozu/config.toml backend add --address 127.0.0.1:3000 --backend-id <my_backend_id> --id <my_cluster_id>
```

### Add http frontend

And an http listener:

```bash
sozu --config /etc/sozu/config.toml listener http add --address 0.0.0.0:80 --tls-versions TLSv1.2 --tls-cipher-list ECDHE-ECDSA-AES256-GCM-SHA384 --tls-cipher-suites TLS_AES_256_GCM_SHA384 --tls-signature-algorithms ECDSA+SHA512 --tls-groups-list x25519 --expect-proxy
```

Finally you have to create a frontend to allow sozu to send traffic from the listener to your backend:

```bash
sozu --config /etc/sozu/config.toml frontend http add --address 0.0.0.0:80 --hostname <my_cluster_hostname> id <my_cluster_id>
```

### Add https frontend

And an https listener:

```bash
sozu --config /etc/sozu/config.toml listener https add --address 0.0.0.0:443
```

Finally you have to create a frontend to allow sozu to send traffic from the listener to your backend:

```bash
sozu --config /etc/sozu/config.toml frontend https add --address 0.0.0.0:443 --hostname <my_cluster_hostname> id <my_cluster_id>
```

## Check the status of sozu

It shows a list of workers and show informations about their statuses.

```bash
sozu --config /etc/sozu/config.toml status
```

## Get metrics and statistics

It will show global statistics about sozu, workers and clusters metrics.

```bash
sozu --config /etc/sozu/config.toml query metrics
```

## Dump and restore state

If sozu configurations (clusters, frontends & backends) are not written in the config file, you can save sozu state to restore it later.

```bash
sozu --config /etc/sozu/config.toml state save --file state.json
```

Then shutdown gracefully sozu:

```bash
sozu --config /etc/sozu/config.toml shutdown
```

Restart sozu and restore its state:

```bash
sozu --config /etc/sozu/config.toml state load --file state.json
```

You should be able to request your cluster like before the shutdown.
