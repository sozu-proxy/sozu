# sozu proxy

## Building

### For OSX build

Mac OS uses an old version of openssl, so we need to use one from Homebrew:

```
brew install openssl
brew link --force openssl
```

If it does not work, set the following environment variables before building:

```
export OPENSSL_LIB_DIR=/usr/local/opt/openssl/lib/
export OPENSSL_INCLUDE_DIR=/usr/local/opt/openssl/include/
```

## Logging

The proxy uses `env_logger`. You can select which module displays logs at which level with an environment variable. Here is an example to display most logs at `info` level, but use `trace` level for the HTTP parser module:

```
RUST_LOG=info,sozu-lib::parser::http11=trace ./target/debug/sozu
```
