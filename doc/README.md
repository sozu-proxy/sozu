# Sōzu

## What is Sōzu?

Sōzu is a reverse proxy for load balancing, written in Rust. Its main job is to balance inbound requests across two or more clusters backends to spread the load.

* It serves as a termination point for TLS sessions. So the workload of dealing with the encryption is offloaded from the backend.

* It can protect the backends by preventing direct access from the network.

* It returns some metrics related to the traffic between clients and backends clusters behind it.

## Introduction

* [Getting started][gs]

* [Configure Sōzu][cg]

* [Configure the Sōzu CLI][cgcli]

* [How to use it][hw]

* [Why you should use Sōzu][ws]

* [Design Motivation][dm]

* [Recipes][r]

## Overview

* [Architecture Overview][ar]

* [Tools & Libraries][tl]

* [Lexicon][lx]

## Operating Sōzu

* [Configure Sōzu][cg]

* [Admin operations & worked examples][cao]

* [Observability — logs, metrics, audit log][ob]

* [Debugging strategies][ds]

* [Benchmarks][bm]

* [Rate limit design][rl]

## Going deeper

* [H2 Mux Internals][h2] — Developer reference for the HTTP/2 multiplexer implementation

* [H2 Mux LIFECYCLE.md][h2lc] — In-tree state-machine reference, maintained alongside the code

* [Health Checks][hc]

* [Lifetime of a session][li]

## Testing

* [Worker upgrade e2e tests][ue]

* [Deterministic simulation (UDP)][uds] — moonpool/VOPR-style seeded fault injection for the sans-io UDP core

* [Nightly CI notes][nci]

## Release Notes

* [Changelog](../CHANGELOG.md)

## Presentations & Slides

* [Sōzu, a hot reconfigurable reverse HTTP proxy by Geoffroy Couprie](https://youtu.be/y4NdVW9sHtU)

* [(FR) Refondre le reverse proxy en 2017 pour faire de l’immutable infrastructure. by Quentin Adam](https://youtu.be/uv3BG1J8YKc)

[gs]: ./getting_started.md
[cg]: ./configure.md
[cgcli]: ./configure_cli.md
[cao]: ./configure_admin_ops.md
[hw]: ./how_to_use.md
[dm]: ./design_motivation.md
[ar]: ./architecture.md
[tl]: ./tools_libraries.md
[lx]: ./lexicon.md
[ws]: ./why_you_should_use.md
[r]: ./recipes.md
[h2]: ./h2_mux_internals.md
[h2lc]: ../lib/src/protocol/mux/LIFECYCLE.md
[hc]: ./health_checks.md
[li]: ./lifetime_of_a_session.md
[ue]: ./upgrade_e2e_tests.md
[uds]: ./udp_simulation.md
[ob]: ./observability.md
[ds]: ./debugging_strategies.md
[bm]: ./benchmark.md
[rl]: ./rate-limit-design.md
[nci]: ./nightly-ci-notes.md
