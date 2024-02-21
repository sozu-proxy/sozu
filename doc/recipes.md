# Recipes: solutions for common problems, tips and tricks

- [Recipes: solutions for common problems, tips and tricks](#recipes-solutions-for-common-problems-tips-and-tricks)
- [Using port 80 or 443 as non root user](#using-port-80-or-443-as-non-root-user)
  - [Capabilities](#capabilities)
  - [iptables](#iptables)
- [High availability architecture](#high-availability-architecture)

# Using port 80 or 443 as non root user

Port numbers under 1024 are usually not accessible to non root users. With sōzu,
we often need to listen on ports 80 (HTTP) and 443 (HTTPS). To avoid running
sōzu as root, here are some solutions to access those ports.

## Capabilities

Recent linux versions (> 2.2) come with a feature called capabilities, that can be
activated depending on the context. To create a listen socket on reserved ports,
we need the `CAP_NET_BIND_SERVICE` capability.

We can set it up by creating an unprivileged `sozu` user, and writing the following
systemd unit file:

```
[Unit]
Description=Sozu - A HTTP reverse proxy, configurable at runtime, fast and safe, built in Rust.
Documentation=https://docs.rs/sozu/
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/bin/sozu start --config /etc/sozu/config.toml
ExecReload=/usr/bin/sozu --config /etc/sozu/config.toml reload
Restart=on-failure
User=sozu
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

It is also possible to give the capability directly to the sozu binary with
`setcap 'cap_net_bind_service=+eip' /usr/bin/sozu`,
but then reserved ports would be accessible by any user than can execute sōzu (so they
could setup of TCP proxy for SSH, SMTP etc to their own software).
The unit file is the recommended way.

## Using unprivileged ports

Different firewalls can be used to route connections from reserved ports to other unprivileged ports.
Most common redirections follow 80 -> 8080 and 443 -> 8443.

### iptables

iptables can be utilized, using a simple nat.

```
iptables -t nat -A PREROUTING -p tcp --dport 80 -j REDIRECT --to-ports 8080
iptables -t nat -A PREROUTING -p tcp --dport 443 -j REDIRECT --to-ports 8443
```

### firewalld

firewalld's syntax is very similiar to iptables. It can be made permanent using `--permanent`.

```
firewall-cmd --direct --add-rule ipv4 nat PREROUTING 0 -p tcp --dport 80 -j REDIRECT --to-port 8080
firewall-cmd --direct --add-rule ipv4 nat PREROUTING 0 -p tcp --dport 443 -j REDIRECT --to-port 8443
```

Note that any software running under the same uid as sōzu will be able to listen on
the 8080 and 8443 ports, because those ports are unprivileged and sōzu sets up
listen socket with the `SO_REUSEPORT` option.

# High availability architecture

TODO

