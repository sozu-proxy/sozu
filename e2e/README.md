# End to end tests

We want to check Sōzu's behavior in all corner cases for the CI.

## Principles

This crate contains thin wrappers around the Sōzu lib, that create:

- a Sōzu worker with a very simple config, able to run a detached thread
- mocked clients that send simple HTTP requests
- mocked backends, sync and async, that reply dummy 200 OK responses (for instance)

This crate provides the `Aggregator` trait, that allows to create simple aggregators.
The aggregators keep track, for instance, of how many requests were received,
and how many were sent, by a backend (just an example).

These elements are instantiated in test functions. They aim at both normal behaviour
(traffic passthrough) but also tackle corner cases, such as:

- smooth soft stop
- immediate hard stop
- behaviour of the zombie checker
- reconnection to a backend

# How to run

The tests are flagged with the usual macros, so they will run with all other tests when you do:

    cargo test

All tests are run one at a time with the `#[serial]` macro. You can run just one using

    cargo test test_issue_810_timeout

If you want to run all e2e tests at once, do:

    cd e2e
    cargo test
