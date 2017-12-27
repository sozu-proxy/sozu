# Architecture

## Master/worker model

### Event loop with mio

> Mio provides a low-level I/O API allowing callers to receive events such as socket read/write > readiness changes.
> Mio give a cross-platform access to the system selector, so that Linux `epoll`, Windows `IOCP`, FreeBSD/Mac OS `kqueue()`, and potentially others can all be used with the same API.
>
> Get more info [here](https://github.com/carllerche/mio).

### Commands

## Proxying

## Load balacing

## SSL

## Logging

# Metrics

## Architecture

## Metrics traced back