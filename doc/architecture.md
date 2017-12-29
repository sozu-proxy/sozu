# Architecture

> This part is mostly for people who want to understand how sozu work and people who want to contribute and help them to understand the architecture.

## Master/worker model

### Event loop with mio

> Mio provides a low-level I/O API allowing callers to receive events such as socket read/write > readiness changes.
> Mio give a cross-platform access to the system selector, so that Linux `epoll`, Windows `IOCP`, FreeBSD/Mac OS `kqueue()`, and potentially others can all be used with the same API.
>
> Get more info [here](https://github.com/carllerche/mio).

TODO

### Commands

TODO

## Proxying

TODO

## Load balacing

TODO

## SSL

TODO

## Logging

TODO

# Metrics

## Architecture

TODO

## Metrics traced back

TODO

## Deep dive

### Buffers

TODO

### Channel

TODO