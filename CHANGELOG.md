# Changelog

## [Unreleased]

### Changed

## 0.11 - 2018-11-15

breaking changes:
- the `public_address` field for listeners is now an `Option<SocketAddr>`, so it's configuration file format is `IP:port` instead of just an IP address
- the `AddHttpsFront` message does not use a certificate fingerprint anymore, so HTTPS frontends do not depend on certificate anymore

### Added

- unit tests checking for structure size changes
- more error handling in sozuctl
- new `automatic_state_save` option to store the configuration state automatically after changes
- event notification system: by sending the `SUBSCRIBE_EVENTS` message, configuration clients can get regular notifications, like messages indicating backend servers are down

### Fixed

- 100 continue behaviour was broken in 0.10 and fixed in 0.10.1
- sticky session cookies are now sent again
- Forwarded headers now indicates correct adresses

## 0.10.0 - 2018-10-25

breaking change: modules have been moved around in sozu-lib

### Added

- sozuctl has a "config check" command
- sozuctl shows the backup flag for backends
- sozuctl shows more info for TCP proxys

### Removed

- sozuctl displays an address column for backends, instead of IP and port

### Changed

- new code organization for sozu-lib, with everything related to protocol implementations in src/protocol
- refactoring of the HTTP protocol implementation
- in preparation for HTTP/2, the pool now handles instances of Buffer, not BufferQueue

### Fixed

- work on TCP proxy stability
- reduce allocations in the HTTP parser
- integer underflow when counting backends in the master state
- get the correct client IP in the HTTPS proxys logs
- do not panic when the client disconnects while we're in the Send proxy protocol implementation

## 0.9.0 - 2018-09-27

### Added

- a futures executor for asynchronous tasks in the master process
- custom 503 page per application

### Changed

- HTTP parser optimizations
- renamed various parts of the code and configuration protocol for more consistency

### Fixed

- upgrade process
- event loop behaviour around abckend connections
- openssl cipher configuration
- circuit breaker


## 0.8.0 - 2018-08-21

- metrics writing fixes
- event loop fixes
- front socket timeout verification
- configuration state verification optimizations
- rustls and openssl configuration fixes
- separate listeners as a specific configuration option
- configuration file refactoring and simplification
- zombie session check

## 0.7.0 - 2018-06-07

- more metrics
- circuit breaking in the TCP proxy

## 0.6.0 - 2018-04-11

- disable debug and trace logs in release builds
- rustls based HTTPS proxy
- ProxyClient trait refactoring
- proxy protocol implementation
- option to send metrics in InfluxDB's tagged format
- PID file


## 0.5.0 - 2018-01-29

- TCP proxy refactoring
- sozuctl UX
- HTTP -> HTTPS redirection
- documentation
- ReplaceCertifacte message


## 0.4.0 - 2017-11-29

- remove mio timeouts
- upgrade fixes
- optimizations

## 0.3.0 - 2017-11-21

- process affinity
- clean system shutdown
- implement 100 continue
- sticky sessions
- build scripts for Fedora, Atchlinux, RPM
- systemd unit file
- metrics
- load balancing algorithms
- retry policy algorithms


### Added

### Changed

### Removed

### Fixed

## 0.2.0 - 2017-04-20

- Event loop refactoring
- contribution guidelines

## 0.1.0 - 2017-04-04

Started implementation:
- TCP proxy
- HTTP proxy
- HTTPS proxy with SNI
- mio based event loop
- configuration diff messages support
- buffer based streaming
- Docker image
- HTTP keep alive
- tested getting configuration events directly from AMQP, was removed
- getting configuration events from a Unix socket
- configuration bootstrap from a TOML file
- logger implementation
- architecture based around master process and worker processes
- control with command line app sozuctl
- command library

[Unreleased]: https://github.com/sozu-proxy/sozu/compare/0.10.0...HEAD
[0.10.0]: https://github.com/sozu-proxy/sozu/compare/0.9.0...0.10.0
[0.9.0]: https://github.com/sozu-proxy/sozu/compare/0.8.0...0.9.0
[0.8.0]: https://github.com/sozu-proxy/sozu/compare/0.7.0...0.8.0
[0.7.0]: https://github.com/sozu-proxy/sozu/compare/0.6.0...0.7.0
[0.6.0]: https://github.com/sozu-proxy/sozu/compare/0.5.0...0.6.0
[0.5.0]: https://github.com/sozu-proxy/sozu/compare/0.4.0...0.5.0
[0.4.0]: https://github.com/sozu-proxy/sozu/compare/0.3.0...0.4.0
[0.3.0]: https://github.com/sozu-proxy/sozu/compare/0.2.0...0.3.0
[0.2.0]: https://github.com/sozu-proxy/sozu/compare/0.1.0...0.2.0
