# Sōzu

> Note: This project is a *work in progress*
> But it will be awesome when it will be ready

# What is Sōzu?

Sōzu is a reverse proxy for load balancing written in Rust. His main job is to balances inbound requests across two or more backends applications to spread the load.

* He serve as a termination point for SSL sessions. So the workload of dealing with the encryption is offloaded from the backend.

* He can protect the backends by preventing direct access from the network.

* Return some statics contents related on the traffics between the clients and the backends applications behind him.


## [Getting started][gs]

## Overview

* [Design Motivation](./overview/design_motivation.md)

* [Architecture Overview](./overview/architecture.md)

* [Tools & Libraries](./overview/tools_libraries.md)

## Going deeper

TODO

## Release Notes

TODO

## Presentations & Slides

* [Sōzu, a hot reconfigurable reverse HTTP proxy by Geoffroy Couprie](https://youtu.be/y4NdVW9sHtU)

* [(FR) Refondre le reverse proxy en 2017 pour faire de l’immutable infrastructure. by Quentin Adam](https://youtu.be/uv3BG1J8YKc)

[gs]: ./getting_started.md