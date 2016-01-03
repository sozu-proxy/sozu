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
