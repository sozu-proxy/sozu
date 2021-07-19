# Design Motivation

This document presents the goals we should have in mind while developing sōzu.
It will inform feature decisions, bugfixes and areas of focus.

## Origin

Sōzu grew from a need at [Clever Cloud](https://www.clever-cloud.com) to handle frequent
configuration changes in a reverse proxy. Since the platform follows immutable infrastructure
principles, applications move frequently from one virtual machine to another, and the
reverse proxy's routing configuration must be updated.

Classical solutions like HAProxy were designed to restart their process on each configuration
change. This causes a few issues:

- restarting a proxy's process will cause a temporary spike in CPU and RAM usage (resource usage is duplicated)
- handling lingering connections:
  - either we stop the old process and lose the connections it was still handling
  - or we keep the old process until connections are done, but we might end up with a lot of old processes if we restart regularly

## Main goals

### Configuration changes at runtime

Changing the proxy's configuration should be a routine task performed at runtime.
We should not need to restart an entire process just to handle this.

This means that configuration management must support dynamic changes, so it must be
reflected in the tools we use to drive sōzu, and we need to provide good libraries
to handle that use case.

### Fine grained modifications

instead of reloading an entire state (like hot relading a configuration file),
we can push small modifications, like "remove this backend server", or
"add this certificate". This gives us more information on the runtime system,
like knowing when we can actually drop a backend server (once no more connections
are using it).

### Bounded resource usage

We should be able to fix the limits in resource usage (CPU, RAM, connections, etc)
directly in sōzu's configuration. Instead of indefinitely increasing resource usage under load,
we will refuse new traffic right away. This provides back pressure and allows clients
to be smart about their retries, and keep a predictable latency and behaviour for
existing connections.

### Never ending software

Once you started sōzu, it should be able to serve traffic indefinitely. If issues
happen that cause a failure, it should not bring down the whole proxy, and it should
be able to go back to handling traffic automatically.

Side effect of this approach: a binary upgrade should not result in lost traffic.

This is the main driver behind the current multiprocess architecture.

## Side goals

### Security

Since sōzu will likely see the traffic from a lot of client connections, and the private
keys to their TLS certificates, it should be safe enough to protect its memory.

To that end, we chose Rust for its memory safety features.

### Predictability

Sōzu should have a very predictable behaviour in its memory usage and its latency.
Any irregularity should be observed and removed.

This standpoint has driven various decisions:

- Rust allowed us to avoid garbage collection pauses
- preallocations and pooling to avoid some allocations in the hot path
- single threaded, shared nothing architecture to avoid synchronization between cores

### The configuration intelligence should be in tools, not inside sōzu

While sōzu is able to handle fine grained configuration changes, it is not its role
to connect to every configuration management tool under the sun (etcd, kubernetes, etc).
Instead, we should provide means to write tools connecting sōzu and the rest of the system.

Going further, the code that handles configuration changes should be reusable outside of sōzu.
It is currently quite easy to write a tool that loads a configuration state from a file,
get the current state from sōzu, generate a diff then send configuration messages for the diff
to sōzu.

Writing configuration tools should be easy enough, and the configuration protocol should be
accessible from any language.
