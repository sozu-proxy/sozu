# Release process

Sozu has a lot of moving pieces and some dependant projects, so

## Checklist

### Documentation

In cases of changes to:

- the configuration file format: update `bin/config.toml` and `doc/how_to_use.md`
- the configuration state: update `lib/src/lib.rs` documentation, and examples in `lib/examples`
- the command messages: update `command/README.md` and `lib/src/lib.rs`

### Testing

### Worker liveness

Launch sozu with `worker_automatic_restart` on, test a request, kill a worker, test a request

### Upgrades

- compile a sozu and sozuctl of the previous published version, store the binaries somewhere
- compile a sozu of the latest version
- start the old sozu version
- query its configuration state
- replace the old sozu file with the new one
- launch an upgrade with sozuctl
- check that the new sozu runs properly (listens for new connections, handles requests, etc)
- check the new sozu's state, compared to the previous one
- verify that the old workers quit correctly (if they were handling WebSocket connections, check that the master detetcs when you kill them)

For minor and patch releases, the upgrade process must always pass correctly.

### Update the changelog

## Publish the new version

update the version number in `Cargo.toml` files in:

- `command/`
- `lib/`
- `bin/`
- `ctl/`
- `futures/`

Run `cargo build` at the root of the project to update the `Cargo.lock` file.

Commit the `Cargo.toml` and `Cargo.lock` changes.

Wait for the Travis CI build to pass.

## Following checks

Verify that the [Docker container](https://hub.docker.com/r/clevercloud/sozu/) was built correctly.

Update related projects to use the latest version of `sozu-command-lib` and `sozu-command-futures`:

- [sozu-acme](https://github.com/sozu-proxy/sozu-acme)
- [sozu-demo](https://github.com/sozu-proxy/sozu-demo)

