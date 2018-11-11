# Sōzu

> Note: This project is a *work in progress*
> But it will be awesome when it will be ready

# What is Sōzu?

Sōzu is a reverse proxy for load balancing written in Rust. Its main job is to balance inbound requests across two or more applications backends to spread the load.

* It serves as a termination point for SSL sessions. So the workload of dealing with the encryption is offloaded from the backend.

* It can protect the backends by preventing direct access from the network.

* Return some metrics related on the traffics between the clients and the backends applications behind it.

# Introduction

* [Getting started][gs]

* [How to use it][hw]

* [Why you should use Sōzu][ws]

* [Design Motivation][dm]

## Overview

* [Architecture Overview][ar]

* [Tools & Libraries][tl]

## Going deeper

TODO

## Release Notes

TODO

## Presentations & Slides

* [Sōzu, a hot reconfigurable reverse HTTP proxy by Geoffroy Couprie](https://youtu.be/y4NdVW9sHtU)

* [(FR) Refondre le reverse proxy en 2017 pour faire de l’immutable infrastructure. by Quentin Adam](https://youtu.be/uv3BG1J8YKc)

[gs]: ./getting_started.md
[hw]: ./how_to_use.md
[dw]: ./design_motivation.md
[ar]: ./architecture.md
[tl]: ./tools_libraries.md
[ws]: ./why_you_should_use.md