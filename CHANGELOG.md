# Changelog

## [Unreleased]

### Changed

## 0.11.16 - 2019-07-23

### Fixed

- detect application level configuration changes in state diff
- TLS 1.3 is now working properly with OpenSSL

## 0.11.15 - 2019-07-19

### Fixed

- pass the app id from HTTP protocol to Pipe protocol when in  websocket

## 0.11.14 - 2019-07-18

### Added

- more info in logs about socket errors

## 0.11.13 - 2019-07-12

### Added

- more logs and metrics around socket errors

### Fixed

- do not clear the metric update flag too soon

## 0.11.12 - 2019-07-10

### Fixed

- add logs-debug and logs-trace options to sozuctl to fix build on Exherbo

## 0.11.11 - 2019-07-09
 
### Added

- send 408 or 504 HTTP errors in case of timeouts
- backend connection time and response time metrics

### Fixed

- test back socket connections before reusing them

### Changed

- a metric is not sent again if its value did not change
- the backend id is added as matedata to backend metrics

## 0.11.10 - 2019-07-04

### Fixed

- test if the backend socket is still valid before reusing it

## 0.11.9 - 2019-06-28

debug release

## 0.11.8 - 2019-06-28

### Fixed

- do not duplicate backend if we modified a backend's parameters

## 0.11.7 - 2019-06-26

### Fixed

- fix infinite loop with front socket

## 0.11.6 - 2019-06-19

### Fixed

- check for existence of the unix logging socket

### Changed

- access log format: indicate if we got the log from HTTP or HTTPS sessions

## 0.11.5 - 2019-06-13

### Added

- will print the session's state if handling it resulted in an infinite loop

### Fixed

- websocket protocol upgrade

## 0.11.4 - 2019-06-07

### Fixed

- wildcard matching

## 0.11.3 - 2019-06-06

### Added

- sozuctl commands to query certificates
- more logs and metrics aroundSNI in OpenSSL

## 0.11.2 - 2019-05-21

### Added

- ActivateListener message for TCP proxies

### Fixed

- wildcard certificate mtching with multiple subdomains in configuration
- diff of TCP listener configuration

## 0.11.1 - 2019-05-06

### Changed

- activate jemallocator and link time optimization
- sozuctl now uses the buffer size defined in the configuration file

### Removed

- procinfo dependency

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
