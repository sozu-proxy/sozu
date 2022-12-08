# Changelog

## [Unreleased]

See milestone [`v0.15.0`](https://github.com/sozu-proxy/sozu/projects/3?card_filter_query=milestone%3Av0.15.0)

## 0.14.2 - 2022-12-08

### 🌟 Features

- Added support of `brotli` (passthrough), see [ [`7d9b560`](https://github.com/sozu-proxy/sozu/commit/7d9b560f36cb64b3c0d06d69a944445254bc3104) ]
- Remove `OpenSSL` in favor of `RusTLS`, see [ [`22bf673`](https://github.com/sozu-proxy/sozu/commit/22bf673ea67df24fcf55bf18ddb5fece4444a2a9) ], [ [`d8f6b30`](https://github.com/sozu-proxy/sozu/commit/d8f6b302072c85a82bb30ebfe7d5eaed4178727f) ], [ [`79755c8`](https://github.com/sozu-proxy/sozu/commit/79755c83081b650401d7047ed700d54826e0e485) ] and [ [`e202d1c`](https://github.com/sozu-proxy/sozu/commit/e202d1c2c0236f0aeccd4b2fc7596640f064d95c) ]
- Merge Sozu ACME implementation, see [ [`339277d`](https://github.com/sozu-proxy/sozu/commit/339277d6f79fa34af4fd5ddc305af5934a82f2cd) ] and [ [`7118e64`](https://github.com/sozu-proxy/sozu/commit/7118e640b321dc72b0f521af4d9fbc53c3610870) ]

### ✍️ Changed

- Update `RPM` packaging, see [ [`0776217`](https://github.com/sozu-proxy/sozu/commit/07762173f210e2fbba2faa7ef48d581e12a4ce20) ], [ [`37e90c2`](https://github.com/sozu-proxy/sozu/commit/37e90c28aa1e75a9476134f5b0fd685dcdfea562) ],
- Update dependencies and refactor a bunch of source code to prepare h2, see [ a lot of commits below 😛 ]

### ⚡ Breaking changes

We remove the support of `OpenSSL` in favor of `RusTLS`, so the tls provider configurations associated to the selection of a tls provider has been dropped as well, see [ [`e202d1c`](https://github.com/sozu-proxy/sozu/commit/e202d1c2c0236f0aeccd4b2fc7596640f064d95c) ]

### Changelog

#### ➕ Added

- [ [`7d9b560`](https://github.com/sozu-proxy/sozu/commit/7d9b560f36cb64b3c0d06d69a944445254bc3104) ] add brotli to encoding header values [`Emmanuel Bosquet`] (`2022-11-29`)
- [ [`22bf673`](https://github.com/sozu-proxy/sozu/commit/22bf673ea67df24fcf55bf18ddb5fece4444a2a9) ] Add support of OpenSSL 3.0.x [`Florentin Dubois`] (`2022-10-17`)
- [ [`d8f6b30`](https://github.com/sozu-proxy/sozu/commit/d8f6b302072c85a82bb30ebfe7d5eaed4178727f) ] Add configuration options for OpenSSL TLS provider [`Florentin Dubois`] (`2022-10-20`)
- [ [`79755c8`](https://github.com/sozu-proxy/sozu/commit/79755c83081b650401d7047ed700d54826e0e485) ] Merged https_openssl and https_rustls [`Eloi DEMOLIS`] (`2022-11-17`)
- [ [`e202d1c`](https://github.com/sozu-proxy/sozu/commit/e202d1c2c0236f0aeccd4b2fc7596640f064d95c) ] Remove OpenSSL [`Eloi DEMOLIS`] (`2022-11-17`)
- [ [`7118e64`](https://github.com/sozu-proxy/sozu/commit/7118e640b321dc72b0f521af4d9fbc53c3610870) ] import https://github.com/sozu-proxy/sozu-acme into the sozu command line [`Emmanuel Bosquet`] (`2022-12-07`)
- [ [`339277d`](https://github.com/sozu-proxy/sozu/commit/339277d6f79fa34af4fd5ddc305af5934a82f2cd) ] update acme-lib and tiny_http dependencies [`Emmanuel Bosquet`] (`2022-12-07`)
- [ [`da0f667`](https://github.com/sozu-proxy/sozu/commit/da0f667d70755ff89dc3f8871d6684e2c898e661) ] Add codeowners file [`Florentin Dubois`] (`2022-11-30`)

#### ✍️ Changed

- [ [`0776217`](https://github.com/sozu-proxy/sozu/commit/07762173f210e2fbba2faa7ef48d581e12a4ce20) ] Making some updates to RPM build spec and script. [`Igal Alkon`] (`2021-07-31`)
- [ [`37e90c2`](https://github.com/sozu-proxy/sozu/commit/37e90c28aa1e75a9476134f5b0fd685dcdfea562) ] Updating the rpm build script to have two modes of build the packages. [`Igal Alkon`] (`2021-07-31`)
- [ [`8997ec2`](https://github.com/sozu-proxy/sozu/commit/8997ec2e31474019e5bdd71ecdc541ccd3308a85) ] Update changelog to add entry for 0.14.1 [`Florentin Dubois`] (`2022-10-13`)
- [ [`073375f`](https://github.com/sozu-proxy/sozu/commit/073375fd4cc67a4d79ae5dfa5627e494b1a4fcca) ] Unit tests, comments and refactoring of command/src/channel.rs [`Emmanuel Bosquet`] (`2022-10-17`)
- [ [`e6a4615`](https://github.com/sozu-proxy/sozu/commit/e6a4615dcbe797ed8c608d1554c3aeb2b2922c45) ] Update README.md to remove `ctl` crate and add `command` one [`Florentin Dubois`] (`2022-10-17`)
- [ [`a930847`](https://github.com/sozu-proxy/sozu/commit/a930847c7b05e831d3680a2e1e7cb461d4b742dd) ] Fix typos [`Kian-Meng Ang`] (`2022-11-10`)
- [ [`6fdfc18`](https://github.com/sozu-proxy/sozu/commit/6fdfc183014bd40f5185a333ba15dd0d0086d210) ] update build scripts with the --locked flag [`Emmanuel Bosquet`] (`2022-12-01`)
- [ [`6f38476`](https://github.com/sozu-proxy/sozu/commit/6f38476a2b036f4143298ca8f2907134d08aa806) ] Update dependencies [`Florentin Dubois`] (`2022-12-01`)
- [ [`ae8ffe7`](https://github.com/sozu-proxy/sozu/commit/ae8ffe78a4100e6c5ec0ea67e7fa12b0f55f0a47) ] Update behaviour of add_certificate to skip already loaded certificate [`Florentin Dubois`] (`2022-12-02`)
- [ [`aef7baa`](https://github.com/sozu-proxy/sozu/commit/aef7baadb427118cb605b5fff379ea1b3283d226) ] Rename variables l to listener on command/src/config [`Florentin Dubois`] (`2022-12-02`)
- [ [`19c4f70`](https://github.com/sozu-proxy/sozu/commit/19c4f7075cd7708f9ca1e3a9b412b1f6e732b957) ] Use rustfmt to format project [`Florentin Dubois`] (`2022-12-02`)
- [ [`72bb233`](https://github.com/sozu-proxy/sozu/commit/72bb2333a62750f5ca59575fd0c299cd186fbbcd) ] fullfill syntax TODOs [`Emmanuel Bosquet`] (`2022-11-17`)
- [ [`dceea6f`](https://github.com/sozu-proxy/sozu/commit/dceea6facf43476661cc9f9c17329069ee6845be) ] log ConnectionError with thiserror [`Emmanuel Bosquet`] (`2022-11-22`)
- [ [`a7697ff`](https://github.com/sozu-proxy/sozu/commit/a7697ff5e358038f0692f6a211763ba86c1aba11) ] Use listener cert when https front lacks cert [`Ion Agorria`] (`2022-11-25`)
- [ [`42c548b`](https://github.com/sozu-proxy/sozu/commit/42c548ba16d3ae1f7e5e49a7b5b333723632ddf6) ] set a TODO to handle EINPROGRESS error when connecting to backend [`Emmanuel Bosquet`] (`2022-11-25`)

#### 🚀 Refactored

- [ [`c882946`](https://github.com/sozu-proxy/sozu/commit/c882946c4816df71b203b1585217cbc446b29d37) ] processing messages between main process and CLI [`Emmanuel Bosquet`] (`2022-10-20`)
- [ [`e90cc52`](https://github.com/sozu-proxy/sozu/commit/e90cc52319343cd43db8d82ec5b3f6dfc89b698c) ] ctl: display response message for successful ProxyRequestOrder [`Emmanuel Bosquet`] (`2022-10-25`)
- [ [`69aac18`](https://github.com/sozu-proxy/sozu/commit/69aac18ca8bfcd2d4a59fdcaa2e5961b6d888f86) ] http session: clean-up session creation [`Emmanuel Bosquet`] (`2022-11-08`)
- [ [`f31c894`](https://github.com/sozu-proxy/sozu/commit/f31c89489524193b17580fafcd2de8972c5f6d92) ] tcp session: clean-up session creation [`Emmanuel Bosquet`] (`2022-11-08`)
- [ [`94994a3`](https://github.com/sozu-proxy/sozu/commit/94994a3652eeafcbec5552cd4ec1086b95c84dda) ] htts_openssl session: clean-up session creation [`Emmanuel Bosquet`] (`2022-11-08`)
- [ [`f5c6cf5`](https://github.com/sozu-proxy/sozu/commit/f5c6cf5dfaa5929ea3f2190ed3951205ebc4653c) ] https_rustls session: clean-up session creation [`Emmanuel Bosquet`] (`2022-11-08`)
- [ [`a4dc22e`](https://github.com/sozu-proxy/sozu/commit/a4dc22ee4b962a4862696d48b296f09ba4a1d8f3) ] remove useless front_socket_mut functions on tcp and http Session [`Emmanuel Bosquet`] (`2022-11-08`)
- [ [`3e2f445`](https://github.com/sozu-proxy/sozu/commit/3e2f4450ac38547d3593f64af60a269a7fc7b4d1) ] full error propagation on ConfigState::handle_order() [`Emmanuel Bosquet`] (`2022-11-16`)
- [ [`42986fd`](https://github.com/sozu-proxy/sozu/commit/42986fd868be27f5244b04c9552d3891c0cacc8e) ] Error propagation on ScmSocket and Channel [`Emmanuel Bosquet`] (`2022-11-17`)
- [ [`edfa1b4`](https://github.com/sozu-proxy/sozu/commit/edfa1b4d7262c4358c5a5ddf4d4ff50f57ee7f53) ] remove obsolete comment about main process crashing [`Emmanuel Bosquet`] (`2022-11-17`)
- [ [`bfc4884`](https://github.com/sozu-proxy/sozu/commit/bfc4884b33c4ad621a053edb016051ef22909e6e) ] replace ConnectionError with anyhow::Result [`Emmanuel Bosquet`] (`2022-12-05`)
- [ [`59bb989`](https://github.com/sozu-proxy/sozu/commit/59bb989ce0ce88da2dd99a7e8ab232314d70e321) ] merge client loop creation logic in the accept_clients function [`Emmanuel Bosquet`] (`2022-12-05`)
- [ [`a6c7c9f`](https://github.com/sozu-proxy/sozu/commit/a6c7c9f077ef9fb4b9cdcd97ed506dabc348f589) ] error propagation on getting 404 and 503 answers from file system [`Emmanuel Bosquet`] (`2022-11-17`)
- [ [`5d83eef`](https://github.com/sozu-proxy/sozu/commit/5d83eefd26ac6e560b999561e3901afb832ac586) ] define the ClusterId type only once [`Emmanuel Bosquet`] (`2022-11-17`)
- [ [`605b95e`](https://github.com/sozu-proxy/sozu/commit/605b95ec593de8efd18696772b0513874f4634fb) ] abstract out the function reset_loop_time_and_get_timeout [`Emmanuel Bosquet`] (`2022-11-25`)
- [ [`2959876`](https://github.com/sozu-proxy/sozu/commit/2959876d11d95d1e22e946e09cd6c54caf7c19d1) ] abstract out the function read_channel_messages_and_notify [`Emmanuel Bosquet`] (`2022-11-25`)
- [ [`7743eba`](https://github.com/sozu-proxy/sozu/commit/7743eba56ddf86ce2f3eeee09bea1d9565e5fa08) ] abstract out the functions zombie_check and shutting_down_complete [`Emmanuel Bosquet`] (`2022-11-25`)
- [ [`c5e2262`](https://github.com/sozu-proxy/sozu/commit/c5e2262ab7d048765ea99a56c94e155660f432ba) ] add fields to Server for a clearer loop flow [`Emmanuel Bosquet`] (`2022-11-25`)
- [ [`96b90c9`](https://github.com/sozu-proxy/sozu/commit/96b90c99df179dd8597908f0d88c4d6234533b01) ] clearer syntax on read_channel_messages_and_notify, comments [`Emmanuel Bosquet`] (`2022-11-28`)
- [ [`1aee08d`](https://github.com/sozu-proxy/sozu/commit/1aee08d034f872bea8a04808841476ea4da74ba5) ] create ConnectionError::MioConnectError [`Emmanuel Bosquet`] (`2022-11-28`)
- [ [`33b3e5f`](https://github.com/sozu-proxy/sozu/commit/33b3e5fcbafbad94daca2520857830a55082663b) ] Abstract out notify proxys (#842) [`Emmanuel Bosquet`] (`2022-11-30`)

#### 💪 First works on H2

- [ [`6bdb937`](https://github.com/sozu-proxy/sozu/commit/6bdb937e60abf63e92633602f1fdec71d930003a) ] Rename HTTPS states, remove Option around State, rename https.rs [`Eloi DEMOLIS`] (`2022-11-17`)
- [ [`a819fa3`](https://github.com/sozu-proxy/sozu/commit/a819fa37edac3fef6b722b6dc51242cd47b6a6c5) ] Add Invalid State in HTTPS, add ALPN to Rustls ServerConfig [`Eloi DEMOLIS`] (`2022-11-17`)
- [ [`26f7d15`](https://github.com/sozu-proxy/sozu/commit/26f7d15dc0613d004ebc78185deabe5c12bfa295) ] Make way for H2 with Alpn switch [`Eloi DEMOLIS`] (`2022-11-17`)
- [ [`ec31f82`](https://github.com/sozu-proxy/sozu/commit/ec31f8235e69e3033cf89a8817664ee6f4ffca27) ] make RouteKey a common struct, make use of its methods [`Emmanuel Bosquet`] (`2022-11-17`)

#### ⛑️ Fixed

- [ [`790f96b`](https://github.com/sozu-proxy/sozu/commit/790f96b49443c85d191bbe3bd83555c485eabda6) ] prevent concurrent testing in the github CI [`Emmanuel Bosquet`] (`2022-12-01`)
- [ [`719b637`](https://github.com/sozu-proxy/sozu/commit/719b637a44f23a63eb09d6a865f0ad95ed76641e) ] ScmSocket::set_blocking updates its blocking field, takes &mut self [`Emmanuel Bosquet`] (`2022-12-01`)
- [ [`2624a43`](https://github.com/sozu-proxy/sozu/commit/2624a430c5d1c02e257a3bf9ddb545f46e29c8fd) ] Fix Sessions close: [`Eloi DEMOLIS`] (`2022-12-02`)

### 🥹 Contributors
* @alkavan made their first contribution in https://github.com/sozu-proxy/sozu/pull/693
* @kianmeng made their first contribution in https://github.com/sozu-proxy/sozu/pull/830
* @Wonshtrum
* @Keksoj
* @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/v0.14.1...0.14.2

## 0.14.1 - 2022-10-13

### 🔖 Documentation

- 💪 Improve documentation about the lifecycle of a session, see [`7e223ee`](https://github.com/sozu-proxy/sozu/commit/7e223eee09b848f599b560f5a6d0dc918e94d20e), [`50a940a`](https://github.com/sozu-proxy/sozu/commit/50a940acad497358c9e60e28d0fed6310d26d290), [`a13eb35`](https://github.com/sozu-proxy/sozu/commit/a13eb3571853f9d1468e6d6a385df2a8d8cb7235) and [`2b6706c`](https://github.com/sozu-proxy/sozu/commit/2b6706c8e3d8ab4c94eefe8d7653cdbd918fc772)
- 🔑 Update TLS cipher suites, see [`3af39a6`](https://github.com/sozu-proxy/sozu/commit/3af39a6fff29d95e0baeccd3a6abfa63354607ce)

### ✍️ Changed

- 🔧 Use Rust edition 2021 and update dependencies, see [`693bc84`](https://github.com/sozu-proxy/sozu/commit/693bc84f0130a7fd6b6f5de3a294ac587bdb26d7), [`8f4449c`](https://github.com/sozu-proxy/sozu/commit/8f4449cd66b85cfa7c7684817115ba13a8762f61), [`e14109b`](https://github.com/sozu-proxy/sozu/commit/e14109bf208e912bfcd8f344b1538cfb02b02c4b), [`339ed21`](https://github.com/sozu-proxy/sozu/commit/339ed2126cf61d79fcb328675a93f14fed9b4b03), [`f064d8b`](https://github.com/sozu-proxy/sozu/commit/f064d8babdab23bb3f64ab7bc400abefa0015358) and [`0e3fffe`](https://github.com/sozu-proxy/sozu/commit/0e3fffeefe2a201c85255eda25b2b85babd65252)

### 📖 Changelog

- [ [`07ccff3`](https://github.com/sozu-proxy/sozu/commit/07ccff35a2176171c3f6d218bdd1622ef2554146) ] Update changelog to add version v0.14.0 [`Florentin Dubois`] (`2022-10-06`)
- [ [`7e223ee`](https://github.com/sozu-proxy/sozu/commit/7e223eee09b848f599b560f5a6d0dc918e94d20e) ] Rewrite session creation in lifetime_of_a_session.md [`Eloi DEMOLIS`] (`2022-10-11`)
- [ [`50a940a`](https://github.com/sozu-proxy/sozu/commit/50a940acad497358c9e60e28d0fed6310d26d290) ] Continue work on the documentation, rename listen_token to listener_token [`Eloi DEMOLIS`] (`2022-10-11`)
- [ [`a13eb35`](https://github.com/sozu-proxy/sozu/commit/a13eb3571853f9d1468e6d6a385df2a8d8cb7235) ] Finish rewriting existing parts [`Eloi DEMOLIS`] (`2022-10-11`)
- [ [`2b6706c`](https://github.com/sozu-proxy/sozu/commit/2b6706c8e3d8ab4c94eefe8d7653cdbd918fc772) ] Finish the lifetime of a session documentation [`Eloi DEMOLIS`] (`2022-10-12`)
- [ [`3af39a6`](https://github.com/sozu-proxy/sozu/commit/3af39a6fff29d95e0baeccd3a6abfa63354607ce) ] Update TLS cipher suites [`Florentin Dubois`] (`2022-10-12`)
- [ [`693bc84`](https://github.com/sozu-proxy/sozu/commit/693bc84f0130a7fd6b6f5de3a294ac587bdb26d7) ] Update rust edition to 2021 [`Florentin Dubois`] (`2022-10-13`)
- [ [`8f4449c`](https://github.com/sozu-proxy/sozu/commit/8f4449cd66b85cfa7c7684817115ba13a8762f61) ] Update command dependencies [`Florentin Dubois`] (`2022-10-13`)
- [ [`e14109b`](https://github.com/sozu-proxy/sozu/commit/e14109bf208e912bfcd8f344b1538cfb02b02c4b) ] Update rust edition to 2021 [`Florentin Dubois`] (`2022-10-13`)
- [ [`339ed21`](https://github.com/sozu-proxy/sozu/commit/339ed2126cf61d79fcb328675a93f14fed9b4b03) ] Update rust edition to 2021 [`Florentin Dubois`] (`2022-10-13`)
- [ [`f064d8b`](https://github.com/sozu-proxy/sozu/commit/f064d8babdab23bb3f64ab7bc400abefa0015358) ] Update binary dependencies [`Florentin Dubois`] (`2022-10-13`)
- [ [`0e3fffe`](https://github.com/sozu-proxy/sozu/commit/0e3fffeefe2a201c85255eda25b2b85babd65252) ] Fix a blocking clippy suggestion [`Florentin Dubois`] (`2022-10-13`)

### 🥹 Contributors
* @Wonshtrum
* @Keksoj
* @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/v0.14.0...0.14.1

## 0.14.0 - 2022-10-06

### 🌟 Features

- 🚀 A new HTTP/1 router, see [`5b58b91`](https://github.com/sozu-proxy/sozu/commit/5b58b91f14b1f98299911b5a3bac4cd0c11a39d2), [`016d89c`](https://github.com/sozu-proxy/sozu/commit/016d89ca72e309499d388464b211aa501e0cf135), [`a9dd46e`](https://github.com/sozu-proxy/sozu/commit/a9dd46e95d9c691e5266e5d476469829e4e1c93a), [`043b928`](https://github.com/sozu-proxy/sozu/commit/043b928e73cf36c11f4a750165bd61ad4284f812), [`c8504d4`](https://github.com/sozu-proxy/sozu/commit/c8504d4d9dab6053b31462c8fa14f1fe32a37e5a), [`5dc5c00`](https://github.com/sozu-proxy/sozu/commit/5dc5c0058192457f540b7614029c4d7279f9dc5b), [`c5b9bfa`](https://github.com/sozu-proxy/sozu/commit/c5b9bfa9d6702e0bc09c92f70f7fe5ed150c92e8), [`f96d1ac`](https://github.com/sozu-proxy/sozu/commit/f96d1ac7a632ee318bb2398b9c891d11eb8058cc), [`af38431`](https://github.com/sozu-proxy/sozu/commit/af38431236243c3183d5b1d75c483e33b8e07c81)
- 🤩 Bootstrap HTTP/2 work, see [`f247ce3`](https://github.com/sozu-proxy/sozu/commit/f247ce3783b912084a330bdf43d861c9c748dfed), [`65d5043`](https://github.com/sozu-proxy/sozu/commit/65d5043f8629744c166994d7e4c71d78983eccf9), [`6da9038`](https://github.com/sozu-proxy/sozu/commit/6da9038355f6c27060ef89a87e6110a33ba9a995), 
- 1️⃣ One command line to rules them all, see [`8555e44`](https://github.com/sozu-proxy/sozu/commit/8555e44987fe3b0939403c24482fb65cbac93bb1)
- ✨ Improve metrics, see [`b7fa649`](https://github.com/sozu-proxy/sozu/commit/b7fa649a5778a498bf096a03b33274fbdf783557), [`8ffca7f`](https://github.com/sozu-proxy/sozu/commit/8ffca7f7e3e812a9bdacd3a08ba19ea564a2d740), [`9e6bd1d`](https://github.com/sozu-proxy/sozu/commit/9e6bd1df2dce563c831fcc71d51d7d0980128aec), [`7515a7e`](https://github.com/sozu-proxy/sozu/commit/7515a7e9d40e17090a9d6d63a23e5a75f3669b55), [`809a481`](https://github.com/sozu-proxy/sozu/commit/809a48130f5408e008d85c04f50a154b9ec5174e), [`67b98d0`](https://github.com/sozu-proxy/sozu/commit/67b98d0dca4814a3476e71b48f5e44db3a2e3c66), [`086fdca`](https://github.com/sozu-proxy/sozu/commit/086fdca72a5d244f44f78b778bd4c20545a4a872), [`3b956fb`](https://github.com/sozu-proxy/sozu/commit/3b956fb15993316de9432762f1704e9ea3cff1cf), [`5691fb9`](https://github.com/sozu-proxy/sozu/commit/5691fb9161182aeb6cd8dd391f54494725ae6881), [`effc0a9`](https://github.com/sozu-proxy/sozu/commit/effc0a91c96f0cd52d1586a3c5c6788271c2a9f9), [`829ad4b`](https://github.com/sozu-proxy/sozu/commit/829ad4b6ad4e294ad4957d7201208384c4676bfa), [`3de4cb7`](https://github.com/sozu-proxy/sozu/commit/3de4cb7981525077524f71a6d2b316abe0ec33b4), [`6dd7f2f`](https://github.com/sozu-proxy/sozu/commit/6dd7f2fddabc668e5f8cf7aeeaf76c8550021316), [`5427107`](https://github.com/sozu-proxy/sozu/commit/54271078eed9220217cfaad73d6344e807f3d1f1), [`c271481`](https://github.com/sozu-proxy/sozu/commit/c2714818685f899bc9883620c67f7f51794aac45), [`7af7a98`](https://github.com/sozu-proxy/sozu/commit/7af7a98a98c918aca5ebfd59ee8fcf190c2f6723), [`d0f853a`](https://github.com/sozu-proxy/sozu/commit/d0f853a6fd4fab9f016f7d4e3ce752ec1908762f), [`aa1176a`](https://github.com/sozu-proxy/sozu/commit/aa1176ac6735187c15154507e55b9e818f542a7c), [`1c2c87a`](https://github.com/sozu-proxy/sozu/commit/1c2c87a9f4001f50163c7ff9c6d558d51d9da0aa), [`5309acb`](https://github.com/sozu-proxy/sozu/commit/5309acb7b385d7ba341eefda110abf7e6d44cf50), [`751c2e8`](https://github.com/sozu-proxy/sozu/commit/751c2e8deaff0e7b92d5963159006e071cc13e98), [`daacf30`](https://github.com/sozu-proxy/sozu/commit/daacf30c735853c41de0e6c1636ebb6ffd02d3d6), [`4f6a47f`](https://github.com/sozu-proxy/sozu/commit/4f6a47fb79c95fc9ca200822956846609213b01f), [`985afc0`](https://github.com/sozu-proxy/sozu/commit/985afc0e3f8ad5fcd0a041d331c903b8760dce06), [`3418b6f`](https://github.com/sozu-proxy/sozu/commit/3418b6f7847c7ca9c9c928c74a77da28b94e0d7f), [`f2d07eb`](https://github.com/sozu-proxy/sozu/commit/f2d07ebd3e32d75e830e86f024a872f7917cf3cf), [`ab34222`](https://github.com/sozu-proxy/sozu/commit/ab342224840a9b3b12edb7cccf06d5f7ec76c80a), [`aaf2224`](https://github.com/sozu-proxy/sozu/commit/aaf2224ed5f9aec4bceee116e6a5626271d9613c), [`f1dec19`](https://github.com/sozu-proxy/sozu/commit/f1dec19515ef286e9d617b45b46a05a75f6a16b3), [`67445c4`](https://github.com/sozu-proxy/sozu/commit/67445c4531b1cbf2316d30d4650a9448e076db7d), [`ae8d9c1`](https://github.com/sozu-proxy/sozu/commit/ae8d9c14e77a9d1e416f90396e01389f567a1066), [`b0844e3`](https://github.com/sozu-proxy/sozu/commit/b0844e37e7ac5f4046e5d592d7d92eaad4d6ffac), [`9eb2c01`](https://github.com/sozu-proxy/sozu/commit/9eb2c016713b69f3a09f0df78f21d363c2bd4f2f), [`e774cff`](https://github.com/sozu-proxy/sozu/commit/e774cff0208bfcb7f862bbfcc2fdc2163fd14a5d), [`81f24c4`](https://github.com/sozu-proxy/sozu/commit/81f24c4a9311d42238abb645c8ffcd763bd3cb48), [`c02b130`](https://github.com/sozu-proxy/sozu/commit/c02b130f45a925fcf316cd5540aeb49e320fd935), [`ce2c764`](https://github.com/sozu-proxy/sozu/commit/ce2c764101fc16d5eb5c6837c5cb6c9cee06999f)
- 🙈 Return a HTTP 503 status code when we reach too many connection, see [`a82a83f`](https://github.com/sozu-proxy/sozu/commit/a82a83f68d6929d6f69b1b355a66581efbffb1f8)
- Unified TLS certificate lifecycle and management, see [`a7952a1`](https://github.com/sozu-proxy/sozu/commit/a7952a10ea7e8e536148d1385026e9f2b28b7a7f), [`bbd4f5f`](https://github.com/sozu-proxy/sozu/commit/bbd4f5fd5af401c8c4d6d20db717297a54037d9b), [`7276e17`](https://github.com/sozu-proxy/sozu/commit/7276e17d27eaf9a70b5da5aff80050ecbfc3ab89) 
- 1️⃣ Set event-based command handling, see [`5f62687`](https://github.com/sozu-proxy/sozu/commit/5f626872f9305b92ec7f9302cdc24785b49d012f), [`3c7ab65`](https://github.com/sozu-proxy/sozu/commit/3c7ab654e3282f8163530cfbbfae3114930593a5), [`d322bc2`](https://github.com/sozu-proxy/sozu/commit/d322bc2e6228e00a2960ea7d2f6f953a50d02249), [`33b6e11`](https://github.com/sozu-proxy/sozu/commit/33b6e11744c256419325427f40a1d625a7588f54), [`5bbde99`](https://github.com/sozu-proxy/sozu/commit/5bbde99bca1e31fe13ac6b78dffbacc05c5d73fe), [`3c24088`](https://github.com/sozu-proxy/sozu/commit/3c24088a2a5670876ef31850db13c6995dc2c73b)
- 💪 Allow to set custom tags on logs, see [`115b9b1`](https://github.com/sozu-proxy/sozu/commit/115b9b1eee9fc94ceedd1e016eda8c2f588155cb), [`a9959ba`](https://github.com/sozu-proxy/sozu/commit/a9959ba34dbe07130170f40204035a1bf14f97ac)

### ⚡ Breaking changes

Between the `v0.13.6` and `v0.14.0` structures present in the `command` folder has changed which led to an incomptibility between those two versions.

### Changelog

#### ➕ Added

- [ [`22f1a72`](https://github.com/sozu-proxy/sozu/commit/22f1a72f71c61a2eb6bd32c3e610b07caddd77f0) ] docs(ctl): add use cases [`Gaël Reyrol`] (`2021-04-19`)
- [ [`c007794`](https://github.com/sozu-proxy/sozu/commit/c0077947b8b0ea1bd2d8d95b0d0f1a39ad770f9a) ] docs: update references to sozuctl [`Gaël Reyrol`] (`2021-04-19`)
- [ [`9e5a2c6`](https://github.com/sozu-proxy/sozu/commit/9e5a2c61f7f0e09074a03a0d2d60a33bc6007264) ] docs(ctl): typo [`Gaël Reyrol`] (`2021-04-19`)
- [ [`06a8e4b`](https://github.com/sozu-proxy/sozu/commit/06a8e4bec0874df165cfcfc115bbe7c9bb64b7af) ] start describing how client sessions are handled [`Geoffroy Couprie`] (`2021-07-30`)
- [ [`f247ce3`](https://github.com/sozu-proxy/sozu/commit/f247ce3783b912084a330bdf43d861c9c748dfed) ] import WIP HTTP/2 implementation [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`65d5043`](https://github.com/sozu-proxy/sozu/commit/65d5043f8629744c166994d7e4c71d78983eccf9) ] change the default buffer size to accomodate HTTP/2 [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6da9038`](https://github.com/sozu-proxy/sozu/commit/6da9038355f6c27060ef89a87e6110a33ba9a995) ] start integrating HTTP/2 in the HTTPS-OpenSSL proxy [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`c5b9bfa`](https://github.com/sozu-proxy/sozu/commit/c5b9bfa9d6702e0bc09c92f70f7fe5ed150c92e8) ] add an Equals path rule [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`b7fa649`](https://github.com/sozu-proxy/sozu/commit/b7fa649a5778a498bf096a03b33274fbdf783557) ] start integrating a KV store for metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`8ffca7f`](https://github.com/sozu-proxy/sozu/commit/8ffca7f7e3e812a9bdacd3a08ba19ea564a2d740) ] merge the Count and Gauge columns in metrics dump [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`9e6bd1d`](https://github.com/sozu-proxy/sozu/commit/9e6bd1df2dce563c831fcc71d51d7d0980128aec) ] aggregate stored metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`855c35e`](https://github.com/sozu-proxy/sozu/commit/855c35eaf4d27975fc8dbba5bec50d4de79f6d03) ] simplify the metrics printer [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`2a2c3b9`](https://github.com/sozu-proxy/sozu/commit/2a2c3b9c6d9f9ec51cb90784ca40273b8b0bd532) ] store an app level metric aggregating backend metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`7515a7e`](https://github.com/sozu-proxy/sozu/commit/7515a7e9d40e17090a9d6d63a23e5a75f3669b55) ] add a separating character for app metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`809a481`](https://github.com/sozu-proxy/sozu/commit/809a48130f5408e008d85c04f50a154b9ec5174e) ] sort cluster ids, backend ids and metric names to keep a consistent view [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`67b98d0`](https://github.com/sozu-proxy/sozu/commit/67b98d0dca4814a3476e71b48f5e44db3a2e3c66) ] add a request counter per cluster and backend [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`086fdca`](https://github.com/sozu-proxy/sozu/commit/086fdca72a5d244f44f78b778bd4c20545a4a872) ] metrics query in command line [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`912508a`](https://github.com/sozu-proxy/sozu/commit/912508a1eeb99546f30eb18deb7116a2fe518cab) ] store cluster and backnd level metrics in different trees [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`3b956fb`](https://github.com/sozu-proxy/sozu/commit/3b956fb15993316de9432762f1704e9ea3cff1cf) ] more structured answers for metrics queries [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5691fb9`](https://github.com/sozu-proxy/sozu/commit/5691fb9161182aeb6cd8dd391f54494725ae6881) ] add the metrics list command [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`effc0a9`](https://github.com/sozu-proxy/sozu/commit/effc0a91c96f0cd52d1586a3c5c6788271c2a9f9) ] add a flag to refresh metrics output [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`829ad4b`](https://github.com/sozu-proxy/sozu/commit/829ad4b6ad4e294ad4957d7201208384c4676bfa) ] store and query time metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`3de4cb7`](https://github.com/sozu-proxy/sozu/commit/3de4cb7981525077524f71a6d2b316abe0ec33b4) ] start aggregation for time metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6dd7f2f`](https://github.com/sozu-proxy/sozu/commit/6dd7f2fddabc668e5f8cf7aeeaf76c8550021316) ] reorder time metric key components [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5427107`](https://github.com/sozu-proxy/sozu/commit/54271078eed9220217cfaad73d6344e807f3d1f1) ] allow queries with timestamps [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`c271481`](https://github.com/sozu-proxy/sozu/commit/c2714818685f899bc9883620c67f7f51794aac45) ] fix count and gauge aggregation [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`7af7a98`](https://github.com/sozu-proxy/sozu/commit/7af7a98a98c918aca5ebfd59ee8fcf190c2f6723) ] no need to aggregate data when clearing [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`d0f853a`](https://github.com/sozu-proxy/sozu/commit/d0f853a6fd4fab9f016f7d4e3ce752ec1908762f) ] deactivate metrics dump for now [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`aa1176a`](https://github.com/sozu-proxy/sozu/commit/aa1176ac6735187c15154507e55b9e818f542a7c) ] do not store time metrics every second [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`1c2c87a`](https://github.com/sozu-proxy/sozu/commit/1c2c87a9f4001f50163c7ff9c6d558d51d9da0aa) ] send the date field [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5309acb`](https://github.com/sozu-proxy/sozu/commit/5309acb7b385d7ba341eefda110abf7e6d44cf50) ] metrics collection can now be disabled at runtime [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`751c2e8`](https://github.com/sozu-proxy/sozu/commit/751c2e8deaff0e7b92d5963159006e071cc13e98) ] clear time metrics like other metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`daacf30`](https://github.com/sozu-proxy/sozu/commit/daacf30c735853c41de0e6c1636ebb6ffd02d3d6) ] reduce logging in metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`a673a3b`](https://github.com/sozu-proxy/sozu/commit/a673a3b6a002be82d3795e29fbe5aee148bf3bfb) ] count config messages loaded at startup [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`4f6a47f`](https://github.com/sozu-proxy/sozu/commit/4f6a47fb79c95fc9ca200822956846609213b01f) ] time metrics can be deactivated independently [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`985afc0`](https://github.com/sozu-proxy/sozu/commit/985afc0e3f8ad5fcd0a041d331c903b8760dce06) ] no need to clear metrics if they are disabled [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`3418b6f`](https://github.com/sozu-proxy/sozu/commit/3418b6f7847c7ca9c9c928c74a77da28b94e0d7f) ] use batching for time metrics insertion [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f2d07eb`](https://github.com/sozu-proxy/sozu/commit/f2d07ebd3e32d75e830e86f024a872f7917cf3cf) ] batch insertion of gauges and counts [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`acb0b06`](https://github.com/sozu-proxy/sozu/commit/acb0b06b469a956c1a6e32c27a7c65c87d01cd55) ] reduce allocations in time metrics handling [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`ab34222`](https://github.com/sozu-proxy/sozu/commit/ab342224840a9b3b12edb7cccf06d5f7ec76c80a) ] move back to in memory storage for metrics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`aaf2224`](https://github.com/sozu-proxy/sozu/commit/aaf2224ed5f9aec4bceee116e6a5626271d9613c) ] simpify store_metric_at [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`a82a83f`](https://github.com/sozu-proxy/sozu/commit/a82a83f68d6929d6f69b1b355a66581efbffb1f8) ] return a HTTP 503 error when there are too many connections [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`a7952a1`](https://github.com/sozu-proxy/sozu/commit/a7952a10ea7e8e536148d1385026e9f2b28b7a7f) ] Create an unified certificate resolver for both https with openssl and rustls [`Florentin Dubois`] (`2022-07-13`)
- [ [`53f6911`](https://github.com/sozu-proxy/sozu/commit/53f6911deb5f2423fa51372279b214071a9189cf) ] added error management in the connection to the socket [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`bbd4f5f`](https://github.com/sozu-proxy/sozu/commit/bbd4f5fd5af401c8c4d6d20db717297a54037d9b) ] Use already parsed certificate and chains in rustls hello method [`Florentin Dubois`] (`2022-07-13`)
- [ [`5f62687`](https://github.com/sozu-proxy/sozu/commit/5f626872f9305b92ec7f9302cdc24785b49d012f) ] make the command server entirely event-based [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`3c7ab65`](https://github.com/sozu-proxy/sozu/commit/3c7ab654e3282f8163530cfbbfae3114930593a5) ] additional error logs and context [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`d322bc2`](https://github.com/sozu-proxy/sozu/commit/d322bc2e6228e00a2960ea7d2f6f953a50d02249) ] edited comments in Command Server, a few bails [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`33b6e11`](https://github.com/sozu-proxy/sozu/commit/33b6e11744c256419325427f40a1d625a7588f54) ] reset the CommandManager channel as nonblocking [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`5bbde99`](https://github.com/sozu-proxy/sozu/commit/5bbde99bca1e31fe13ac6b78dffbacc05c5d73fe) ] Implements command parser with serde error propagation [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`3c24088`](https://github.com/sozu-proxy/sozu/commit/3c24088a2a5670876ef31850db13c6995dc2c73b) ] Split ctl/command.rs into modules [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`7276e17`](https://github.com/sozu-proxy/sozu/commit/7276e17d27eaf9a70b5da5aff80050ecbfc3ab89) ] Update certificate replacement behaviour (#762) [`Florentin DUBOIS`] (`2022-07-13`)
- [ [`9b14a65`](https://github.com/sozu-proxy/sozu/commit/9b14a65ea0d05e811f106fc8ff42ea1226842e4d) ] Add support of openssl 1.1.1 [`Florentin Dubois`] (`2022-07-13`)
- [ [`115b9b1`](https://github.com/sozu-proxy/sozu/commit/115b9b1eee9fc94ceedd1e016eda8c2f588155cb) ] Implements custom tags on access logs of protocols TCP, HTTP and HTTPS [`Emmanuel Bosquet`] (`2022-07-29`)
- [ [`a9959ba`](https://github.com/sozu-proxy/sozu/commit/a9959ba34dbe07130170f40204035a1bf14f97ac) ] Add reference counting on for listeners on proxy [`Florentin Dubois`] (`2022-08-03`)
- [ [`f1dec19`](https://github.com/sozu-proxy/sozu/commit/f1dec19515ef286e9d617b45b46a05a75f6a16b3) ] fix metrics enabling, disabling and clear on the CLI and Command Server [`Emmanuel Bosquet`] (`2022-08-09`)
- [ [`67445c4`](https://github.com/sozu-proxy/sozu/commit/67445c4531b1cbf2316d30d4650a9448e076db7d) ] store cluster metrics in a simpler way, query them all [`Emmanuel Bosquet`] (`2022-08-19`)
- [ [`ae8d9c1`](https://github.com/sozu-proxy/sozu/commit/ae8d9c14e77a9d1e416f90396e01389f567a1066) ] retrieve cluster and backend metrics by ids [`Emmanuel Bosquet`] (`2022-08-22`)
- [ [`b0844e3`](https://github.com/sozu-proxy/sozu/commit/b0844e37e7ac5f4046e5d592d7d92eaad4d6ffac) ] filter metrics by metric name [`Emmanuel Bosquet`] (`2022-08-23`)
- [ [`9eb2c01`](https://github.com/sozu-proxy/sozu/commit/9eb2c016713b69f3a09f0df78f21d363c2bd4f2f) ] clear the metrics LocalDrain every plain hour [`Emmanuel Bosquet`] (`2022-08-24`)
- [ [`e774cff`](https://github.com/sozu-proxy/sozu/commit/e774cff0208bfcb7f862bbfcc2fdc2163fd14a5d) ] error management in metrics recording and retrieving [`Emmanuel Bosquet`] (`2022-08-24`)
- [ [`81f24c4`](https://github.com/sozu-proxy/sozu/commit/81f24c4a9311d42238abb645c8ffcd763bd3cb48) ] gather and display main process metrics [`Emmanuel Bosquet`] (`2022-08-24`)
- [ [`c02b130`](https://github.com/sozu-proxy/sozu/commit/c02b130f45a925fcf316cd5540aeb49e320fd935) ] refactor metrics query format and CLI metric command [`Emmanuel Bosquet`] (`2022-08-24`)
- [ [`ce2c764`](https://github.com/sozu-proxy/sozu/commit/ce2c764101fc16d5eb5c6837c5cb6c9cee06999f) ] cli table, nice formatting [`Emmanuel Bosquet`] (`2022-08-29`)

#### ✍️ Changed

- [ [`6382efd`](https://github.com/sozu-proxy/sozu/commit/6382efdca78c1cd644c3725876a35fee14d885b0) ] refactor: reorganize docs and typo [`Gaël Reyrol`] (`2021-04-16`)
- [ [`803b482`](https://github.com/sozu-proxy/sozu/commit/803b48296ed45289f5197e9f2a57ceeafe0599f5) ] refactor: typo and move some blocks [`Gaël Reyrol`] (`2021-04-16`)
- [ [`8a530c2`](https://github.com/sozu-proxy/sozu/commit/8a530c278e3984fb03bd05091f1cf07b952c43a6) ] initialize the logger before writing the pid file [`Emmanuel Bosquet`] (`2021-07-30`)
- [ [`3f98117`](https://github.com/sozu-proxy/sozu/commit/3f98117ab322b57cb7d10badf89dd2fd0885bdfc) ] rewrite start function with beautiful error handling [`Emmanuel Bosquet`] (`2021-07-30`)
- [ [`7628b46`](https://github.com/sozu-proxy/sozu/commit/7628b46f4b89b64c10a288995b86aeb60476d0ab) ] more readable error handling on write_pid_file() [`Emmanuel Bosquet`] (`2021-07-30`)
- [ [`4ce87cb`](https://github.com/sozu-proxy/sozu/commit/4ce87cb567dd384d0d71c2f5e6ac0d6fc136f636) ] take review into account, return errors instead of Ok(()) [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`4cd98ff`](https://github.com/sozu-proxy/sozu/commit/4cd98ff575dde071dd5c4cc3ca48b611689af27e) ] minor fixes to sozuctl, command.rs [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`3b38893`](https://github.com/sozu-proxy/sozu/commit/3b3889314d1db6189aab5a4da27ccdf6aeb4a02b) ] added with_context() and map_err() to error management of sozuctl [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`387fe2b`](https://github.com/sozu-proxy/sozu/commit/387fe2b6daea0eab810cc1ae6e168478046d8468) ] more harmonious, systematic error handling [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`7979942`](https://github.com/sozu-proxy/sozu/commit/79799422b8c24c541e1dcf4eb7080dd9cf19281f) ] better follow-up of worker RunState [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`5cb34ac`](https://github.com/sozu-proxy/sozu/commit/5cb34ac8ed473f4219a84bcffe43bd7a2268b873) ] comments about what is to change [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`863ee59`](https://github.com/sozu-proxy/sozu/commit/863ee59e405d181d729974a45cd1f737b27fe2cb) ] added frustrated comments about code making no sense [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`6b24a24`](https://github.com/sozu-proxy/sozu/commit/6b24a24b2541d5944e3862cfd00bd006da27224a) ] proper handling of WorkerClose by the CommandServer [`Emmanuel Bosquet`] (`2021-08-18`)
- [ [`7cddf2c`](https://github.com/sozu-proxy/sozu/commit/7cddf2ca9c1e28860f7f2500d2b5d0b76613d5ce) ] add a test for protocol upgrades [`Geoffroy Couprie`] (`2021-08-20`)
- [ [`78bf601`](https://github.com/sozu-proxy/sozu/commit/78bf601e72b09684c39af9f69e3fd7d8caf97f5d) ] update to nom 7.0, remove the last macros [`Geoffroy Couprie`] (`2021-08-23`)
- [ [`9c551d6`](https://github.com/sozu-proxy/sozu/commit/9c551d6e6a3cd0bd090f43f84724eeff52dd5721) ] describe what happens when reading and parsing the request [`Geoffroy Couprie`] (`2021-08-25`)
- [ [`1c0f75a`](https://github.com/sozu-proxy/sozu/commit/1c0f75af2474e84133b8ca37a08e7e19f395b8de) ] Replace the TODO "Why you should use Sōzu?" [`Arnaud Lemercier`] (`2021-11-08`)
- [ [`7fb636f`](https://github.com/sozu-proxy/sozu/commit/7fb636fe52596cb3897f1cedcaa79d1a9861ec77) ] allow Dockerfile to choose Alpine base version (#755) [`Mickaël Wolff`] (`2022-02-21`)
- [ [`5b58b91`](https://github.com/sozu-proxy/sozu/commit/5b58b91f14b1f98299911b5a3bac4cd0c11a39d2) ] replace the trie with a tree of hashmaps [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`96c0c5c`](https://github.com/sozu-proxy/sozu/commit/96c0c5c0f57ceaa2a1c8196738b738a949bcf7cd) ] create the router module [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`016d89c`](https://github.com/sozu-proxy/sozu/commit/016d89ca72e309499d388464b211aa501e0cf135) ] add a variant of the trie that supports regexp matches [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`4fa109c`](https://github.com/sozu-proxy/sozu/commit/4fa109cbb93d4dee8526ea5dc1bf803b1965d689) ] remove debug logs [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`a9dd46e`](https://github.com/sozu-proxy/sozu/commit/a9dd46e95d9c691e5266e5d476469829e4e1c93a) ] implement the new Router [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`043b928`](https://github.com/sozu-proxy/sozu/commit/043b928e73cf36c11f4a750165bd61ad4284f812) ] add new routing rules to configuration messages [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`c8504d4`](https://github.com/sozu-proxy/sozu/commit/c8504d4d9dab6053b31462c8fa14f1fe32a37e5a) ] use the new router and integrate in sozuctl [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`8a2a17c`](https://github.com/sozu-proxy/sozu/commit/8a2a17ca6a541f916f592a8191fc292a2e3e4455) ] ignore wildcard case in quickcheck tests [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`3822c5a`](https://github.com/sozu-proxy/sozu/commit/3822c5aea55b61d6b760fe186f4ab3b83beb0f92) ] update to cookie-factory 0.3 [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`8555e44`](https://github.com/sozu-proxy/sozu/commit/8555e44987fe3b0939403c24482fb65cbac93bb1) ] merge sozuctl in sozu [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5dc5c00`](https://github.com/sozu-proxy/sozu/commit/5dc5c0058192457f540b7614029c4d7279f9dc5b) ] add deny rules [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`270bfee`](https://github.com/sozu-proxy/sozu/commit/270bfeefb43303f092383dfdc982c669670e8e48) ] rename HttpFront to HttpFrontend [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`cf59369`](https://github.com/sozu-proxy/sozu/commit/cf593699ce2ccabe4cf6a1aa63d909056d8f4ecb) ] rename CertFingerprint to CertificateFingerprint [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f4f05b4`](https://github.com/sozu-proxy/sozu/commit/f4f05b46267b30bcb634426f2935a24c78091182) ] rename Application to Cluster [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`d2cf976`](https://github.com/sozu-proxy/sozu/commit/d2cf9764b0a6fe89398f0bd96e79ab4fb73caf7a) ] the HTTP frontends hashmap key should serialize to a string [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`95909ea`](https://github.com/sozu-proxy/sozu/commit/95909eaeabde905af11ded7ea27144687cefeb00) ] rename front to address [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`9d22c12`](https://github.com/sozu-proxy/sozu/commit/9d22c12dd4c777d662472a48004be3fcdf85c9e5) ] custom PartialOrd and Ord implementations for SocketAddr [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`1fff30d`](https://github.com/sozu-proxy/sozu/commit/1fff30de4484477d5347fbaa578bfe333602efeb) ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f96d1ac`](https://github.com/sozu-proxy/sozu/commit/f96d1ac7a632ee318bb2398b9c891d11eb8058cc) ] add routing based on the HTTP method [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`af38431`](https://github.com/sozu-proxy/sozu/commit/af38431236243c3183d5b1d75c483e33b8e07c81) ] make the method configurable through the cli [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`a6e7c72`](https://github.com/sozu-proxy/sozu/commit/a6e7c7210fbc6042639bdcd6d41798ed28ec2ed7) ] move target_to_backend to sozu-command-lib [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`b66b0c8`](https://github.com/sozu-proxy/sozu/commit/b66b0c805e9cd462339473da9bf827e17348b06e) ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f12bacb`](https://github.com/sozu-proxy/sozu/commit/f12bacbc340087c7da11dfe15487961bc96ec8e2) ] sled can create directly in a temporary file [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f079127`](https://github.com/sozu-proxy/sozu/commit/f079127410b78c0a0bec54c76d01ee3921d7ade8) ] change key format [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`194908f`](https://github.com/sozu-proxy/sozu/commit/194908fbc86d5758210d21580effd743e07c2248) ] cosmetics [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6a853ce`](https://github.com/sozu-proxy/sozu/commit/6a853ce0673d6f9608cce63fc6627c4f1c28ce4c) ] more logs [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`627dd0e`](https://github.com/sozu-proxy/sozu/commit/627dd0ead5e83bae362e4dfbe0e5958f55262e44) ] rusfmt pass [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`2e723bc`](https://github.com/sozu-proxy/sozu/commit/2e723bcc4ff7775159f49a0df4aa2a7b8f880d44) ] edition 2021 [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`2cfeff7`](https://github.com/sozu-proxy/sozu/commit/2cfeff7ad754de21de8f26fba10c941359cc047f) ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`0328f0e`](https://github.com/sozu-proxy/sozu/commit/0328f0e8b50834fe4833140060de54d7204da963) ] update dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`92ded4a`](https://github.com/sozu-proxy/sozu/commit/92ded4a542b260cd5bca6eca9e9cb335c9e21594) ] store a mio::Registry in ProxyConfiguration implementations [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`538741c`](https://github.com/sozu-proxy/sozu/commit/538741ccfe65570952e3eb4de534046e6a177d91) ] remove the poll argument from ProxyConfiguration methods [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`e217e03`](https://github.com/sozu-proxy/sozu/commit/e217e037538d172561d044c170822c344c0df73b) ] store the sessions slab in a Rc<RefCell<>> [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`3e6ee85`](https://github.com/sozu-proxy/sozu/commit/3e6ee858bc02f9b5824cdffae5a00a1f3ebd579e) ] store the sessions slab in ProxyConfiguration implementations [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`72ccfe9`](https://github.com/sozu-proxy/sozu/commit/72ccfe9ee28f92cd6388b034a3180aed943d5700) ] now create_session uses the internal slab instance [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`7eef4f0`](https://github.com/sozu-proxy/sozu/commit/7eef4f0e1fc0bc75bccdebc61657d209de7b4946) ] use the internal slab instance in connect_to_backend [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`ad26bb8`](https://github.com/sozu-proxy/sozu/commit/ad26bb895e28d984f6225108645529db7b4bfa49) ] move slab capacity check in connect_to_backend [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`0716c42`](https://github.com/sozu-proxy/sozu/commit/0716c42cf6feeb5004f364ae16890cdad871d488) ] factor data in the new SessionManager object [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f5b8290`](https://github.com/sozu-proxy/sozu/commit/f5b8290d664875b3ce1f7e3947809f7b8912a231) ] refactor session management [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`56e6579`](https://github.com/sozu-proxy/sozu/commit/56e6579dd32e3bb476a496f36b7dcb680fff17a5) ] simplify session creation [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`a152063`](https://github.com/sozu-proxy/sozu/commit/a15206320d748055c912ee0a859768b699e0a923) ] move close_session() to the session manager [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`8a9bbc9`](https://github.com/sozu-proxy/sozu/commit/8a9bbc9a3d93063a9c9ef3d7af77691e14af51b2) ] pass a Rc<Refcell<Proxy>> as argument to create_session [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`4ef0c60`](https://github.com/sozu-proxy/sozu/commit/4ef0c607d304b2283d1b21810e3320cef294725f) ] store an instance of Proxy in sessions, handle close() in ready() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`77e36b8`](https://github.com/sozu-proxy/sozu/commit/77e36b87f453d77ca20678a12b189829328c1527) ] implement CloseBackend in sessions [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`11bf1c8`](https://github.com/sozu-proxy/sozu/commit/11bf1c88491636be50acc319b2b3ae205810c966) ] the circuit breaker check should be in the session [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`8f1b0cc`](https://github.com/sozu-proxy/sozu/commit/8f1b0cc6a24021145c9e28e48c8d0a7925392975) ] move some checks to the session object [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`e2f40a5`](https://github.com/sozu-proxy/sozu/commit/e2f40a5fb76f5fec99ab29a25bbb8e3cc3ac0ad1) ] simplify backend_from_request [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`1685d8f`](https://github.com/sozu-proxy/sozu/commit/1685d8fd48d5d7def53fcbc7f19ee81910a8b031) ] pass the session to the ready() method [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6b7681b`](https://github.com/sozu-proxy/sozu/commit/6b7681b28145b055155a72ff6342a790604def00) ] move connect_to_backend to the session, call it from ready() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6310f19`](https://github.com/sozu-proxy/sozu/commit/6310f19affc4645df0dcf5c4afbd46686c2a4711) ] implement reconnect_to_backend in sessions [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`c6ce2fd`](https://github.com/sozu-proxy/sozu/commit/c6ce2fd21c699e8239d49c29bf16015129ddafd0) ] remove connect_to_backend from ProxyConfiguration [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`59c0548`](https://github.com/sozu-proxy/sozu/commit/59c0548432dabacb57d57aa629d5bf105cb30d0b) ] remove the ProxySessionCast trait [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`8a801cf`](https://github.com/sozu-proxy/sozu/commit/8a801cf752d1bc0956b60e17404a7710b6b9d2bc) ] deregister the back socket inside close_backend_inner() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`fc8daa4`](https://github.com/sozu-proxy/sozu/commit/fc8daa4a2b552ffbd46873a7060552c408194441) ] deregister the front socket inside close_inner [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`3151dea`](https://github.com/sozu-proxy/sozu/commit/3151dea7be521fd57ae384cef5cfb9b96f6fd1c9) ] replace ProxySession::close() with close_inner(), remove close_session() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6c2da60`](https://github.com/sozu-proxy/sozu/commit/6c2da60f3720dd40e2aeeae7343d7b63a9e51f34) ] remove ProxySession::close_backend() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`c573f40`](https://github.com/sozu-proxy/sozu/commit/c573f409cb37e32543d55c4e6b0dce1861340851) ] handle session close in ProxySession::timeout() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6dd1f30`](https://github.com/sozu-proxy/sozu/commit/6dd1f30cb02241b94441d7d188889008a0d489ea) ] remove interpret_session_order() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`c74fb59`](https://github.com/sozu-proxy/sozu/commit/c74fb59534bd9d126e0c04920c2872cc22e89f82) ] handle session close in ProxySession::shutting_down() [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`61cb878`](https://github.com/sozu-proxy/sozu/commit/61cb87884ab54f9f7f1467c757a3a642adafa54f) ] clean up some warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f995e45`](https://github.com/sozu-proxy/sozu/commit/f995e45886c3f4c9ae3a0e9b9ce36f887e5fd4dd) ] handle ConnectBackend and ReconnectBackend in ready_inner [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f78c9c6`](https://github.com/sozu-proxy/sozu/commit/f78c9c63da6d5ffc003e7f0e0c2b3519f7ad550f) ] remove a warning and a debug log [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`09a75ba`](https://github.com/sozu-proxy/sozu/commit/09a75bae61f4c8a39e0a2e2b08e74292e2c7fa31) ] anyhow almost everywhere [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`c7fe82d`](https://github.com/sozu-proxy/sozu/commit/c7fe82da818c5933c0945de11f39ab72538fe692) ] remove returned anyhow::Result from CommandServer::run() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`b7e34cf`](https://github.com/sozu-proxy/sozu/commit/b7e34cf0e51a33c72566002dadc060f9889de71a) ] anyhow error management in main.rs [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`047c03a`](https://github.com/sozu-proxy/sozu/commit/047c03a367fae0b749527ef417f00ff45124bfdc) ] clearer WorkerClose syntax [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`8512878`](https://github.com/sozu-proxy/sozu/commit/8512878664ae4e24fcf22d8092033600f4a5a5b8) ] better syntax and error management in launch_worker() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`af29ec6`](https://github.com/sozu-proxy/sozu/commit/af29ec6889c0b30869e660104c0fbde79cb34d2a) ] add a loop in bin/src/ctl::upgrade_main() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`c18a540`](https://github.com/sozu-proxy/sozu/commit/c18a540ed6a88988500084e2efdff7ea76f93ea3) ] initialize logging in main.rs so that ctl() benefits from it [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`da3cdc0`](https://github.com/sozu-proxy/sozu/commit/da3cdc0281ddc78c54c080a54f780393c91eb4d9) ] initialize logging in start() and in ctl() but not in main [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`d74ff84`](https://github.com/sozu-proxy/sozu/commit/d74ff84b48b55f29758de9420301d132a22a1289) ] handle_worker_close() method [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`22ae844`](https://github.com/sozu-proxy/sozu/commit/22ae84490d6c9299269aebb0f26e4016c771d329) ] some logging in ctl/command::upgrade_worker() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`a854c77`](https://github.com/sozu-proxy/sozu/commit/a854c775437ce2903cd913b7c3f72c430b56cee8) ] small corrections for review, adding error handling and removing useless code [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`92fc1bf`](https://github.com/sozu-proxy/sozu/commit/92fc1bf3954dab921b62fd260306ce1fbe12f2d8) ] do not borrow the sessions slab while calling a session timeout [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`ec9fdf8`](https://github.com/sozu-proxy/sozu/commit/ec9fdf8324ad34f1ac46c1c383a6a3d111db1c6f) ] minor config file change [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`3036749`](https://github.com/sozu-proxy/sozu/commit/30367493f8007c2e62f8febb0e7be671999e2b06) ] more verbose cli logging command [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`6b141e0`](https://github.com/sozu-proxy/sozu/commit/6b141e008720a69cae94feeb83fd0a8da033581f) ] Add basic frontend list subcommand and hello world response [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`e0b94bf`](https://github.com/sozu-proxy/sozu/commit/e0b94bf2454fc46229a9b07af5ae92d42ed411b2) ] added filtering of frontends by domain name [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`ee64329`](https://github.com/sozu-proxy/sozu/commit/ee643293776cbd1a4530cfc749be92c006e80a90) ] better syntax on panick safeguard [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`7f799d7`](https://github.com/sozu-proxy/sozu/commit/7f799d7002384b77161b9876d646ae12b7e5351e) ] update most dependencies [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`730d901`](https://github.com/sozu-proxy/sozu/commit/730d901a649fe10d8daf13f7a12158bae5f80d1a) ] comment out randomly failing test for now [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`57b07a4`](https://github.com/sozu-proxy/sozu/commit/57b07a44855affea0150f9d25087b4afeef4bf7b) ] don't use deprecated mio features [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`64715c7`](https://github.com/sozu-proxy/sozu/commit/64715c70a1c9912548dd142ad97b0718ba4d6268) ] switch to socket2 [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`b1fb359`](https://github.com/sozu-proxy/sozu/commit/b1fb35967db8813a22d46b8e3f64fab878aa07cf) ] silence warnings [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`13f153c`](https://github.com/sozu-proxy/sozu/commit/13f153cc7fca5ab03c22e50d65a86443d8969055) ] update to rustls 0.20 [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`d56536a`](https://github.com/sozu-proxy/sozu/commit/d56536a6b6a01b21add90242ad1a9bd8e23b4b1c) ] wrappring channel.read_message() with a timeout function [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`0ca805e`](https://github.com/sozu-proxy/sozu/commit/0ca805eef5d4b1a9ff6d58eecdca0b58d2af8355) ] Created read_message_blocking_timeout() method on Channel [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`48e9493`](https://github.com/sozu-proxy/sozu/commit/48e9493bf6b86c60bbbb03c47c9bfb738fcb6564) ] more verbose worker upgrade error [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`5629cca`](https://github.com/sozu-proxy/sozu/commit/5629ccabe350be4b7051f8f49ab9c1d0675f7d8f) ] added proper timeout to upgrade_worker() call in upgrade_main() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`8f73aa1`](https://github.com/sozu-proxy/sozu/commit/8f73aa1d6555decdc3d52dcfd2f388dc290642c9) ] update mio to 0.8 [`Marc-Antoine Perennou`] (`2022-07-13`)
- [ [`78272fc`](https://github.com/sozu-proxy/sozu/commit/78272fcc9702a517b55672dc0b127aa44034a631) ] error logging on getting saved_state from the config [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`d9e008f`](https://github.com/sozu-proxy/sozu/commit/d9e008f2db9b4062dd209737609e267af8530b3a) ] comment in config.toml that the path of saved_state should be relative [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`fba2f8e`](https://github.com/sozu-proxy/sozu/commit/fba2f8e6fdcd2e47368d0488895dba21eedf689d) ] adapt unit test of Config::load_from_path() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`c4ec939`](https://github.com/sozu-proxy/sozu/commit/c4ec939f2b51b2584f42768b5eeb5d56310a6ccf) ] handle_client_message returns Result<OrderSuccess> [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`c9d0c6c`](https://github.com/sozu-proxy/sozu/commit/c9d0c6c73181a40037712e621c19adbb8c599df8) ] beautify use statements and CommandServer impl blocks [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`9d60cd8`](https://github.com/sozu-proxy/sozu/commit/9d60cd8aa624bde7d86640f81c28f67555059c3d) ] Apply clippy suggestions using rust edition 2021 [`Florentin Dubois`] (`2022-07-13`)
- [ [`e05b80e`](https://github.com/sozu-proxy/sozu/commit/e05b80eba26693748ac2c32327ff44b7638043cb) ] segregate the state parsing logic into parse_state_data() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`9d90d45`](https://github.com/sozu-proxy/sozu/commit/9d90d45e4679f658e5e94966d06756067c07bfe7) ] Revert "segregate the state parsing logic into parse_state_data()" [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`11f916d`](https://github.com/sozu-proxy/sozu/commit/11f916ddb6c5e571044faf005e04d90db49e37e6) ] Format all use statements (#749) [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`5aa0ee1`](https://github.com/sozu-proxy/sozu/commit/5aa0ee1325ce89943afc95e569790add31c990ad) ] sort use statements in files of main process (#750) [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`fdcbacc`](https://github.com/sozu-proxy/sozu/commit/fdcbacc7548d5029f9c30b34a7ed20ed3eacf21b) ] commented the worker and client loops, renamed variables (#752) [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`adf2d3a`](https://github.com/sozu-proxy/sozu/commit/adf2d3a72ff4b57a4a122c3bdad48ede09424a74) ] segregate the log level changing logic into its own function [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`ce80563`](https://github.com/sozu-proxy/sozu/commit/ce805638462ebf018b8ff48b5a53a086d70ab1f4) ] better variable naming and comments in CommandServer::worker_order() [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`1b3cd3c`](https://github.com/sozu-proxy/sozu/commit/1b3cd3c79490d6807e90583f54dfc081e771d513) ] Update workspace dependencies [`Florentin Dubois`] (`2022-07-13`)
- [ [`da2adcf`](https://github.com/sozu-proxy/sozu/commit/da2adcf67b9aa026b8bd2b544ad2f952afed13b4) ] Update command, lib and binaries dependencies [`Florentin Dubois`] (`2022-07-13`)
- [ [`697af1d`](https://github.com/sozu-proxy/sozu/commit/697af1d38f93e40fef55eee0a6bcb9ab5e8f2559) ] respond with ProxyResponseStatus::Error instead of panic when no listener is found [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`15bd0fd`](https://github.com/sozu-proxy/sozu/commit/15bd0fd19b76ff57cd5baba6fae465fd0f3071cd) ] constructor functions for ProxyResponse [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`a4e7dec`](https://github.com/sozu-proxy/sozu/commit/a4e7dec4d23dbcaa462c09a28ef4a5b6f13cbbaf) ] remove if let statements from server::run and some session logic [`Emmanuel Bosquet`] (`2022-07-20`)
- [ [`17c376a`](https://github.com/sozu-proxy/sozu/commit/17c376afcecc47bde78dc926ce60670267e9e10b) ] refactor certificate logic in ctl, with Results instead of Options [`Emmanuel Bosquet`] (`2022-07-22`)
- [ [`11bda07`](https://github.com/sozu-proxy/sozu/commit/11bda07680c090b4e898e92e28580cb44969b3f9) ] Remove all nonbreakable spaces [`Emmanuel Bosquet`] (`2022-07-29`)
- [ [`9054d9c`](https://github.com/sozu-proxy/sozu/commit/9054d9c6ca31cf70fbf8aa437fbd8b44eee4f400) ] Use matching pattern and Entry enum to add listener [`Florentin Dubois`] (`2022-08-03`)
- [ [`a8dde73`](https://github.com/sozu-proxy/sozu/commit/a8dde73027dc5dcf15e620e1acf6322d23f310dc) ] Use std::collections::HashMap instead of hashbrown::HashMap [`Florentin Dubois`] (`2022-08-03`)
- [ [`55b6c58`](https://github.com/sozu-proxy/sozu/commit/55b6c580cecde379d171cbdae88d8278f037015c) ] Use clap 3 with derive instead of StructOpt [`Florentin Dubois`] (`2022-08-03`)
- [ [`53a47ae`](https://github.com/sozu-proxy/sozu/commit/53a47ae426648fc9f0d42781fb8360c093f60e8b) ] Fix command line arguments conflicts [`Florentin Dubois`] (`2022-08-04`)
- [ [`8004c76`](https://github.com/sozu-proxy/sozu/commit/8004c76da17f664d35444ab1dfa10e2fa20b34b3) ] correct command line tutorial in doc/configure_cli [`Emmanuel Bosquet`] (`2022-08-04`)
- [ [`3403cc4`](https://github.com/sozu-proxy/sozu/commit/3403cc4792ec30068bcbc08d3ee84f88cbb1417e) ] Update dependencies [`Florentin Dubois`] (`2022-08-08`)
- [ [`4f8eb4b`](https://github.com/sozu-proxy/sozu/commit/4f8eb4ba1f527314a44a4d9fb5176ef86b052dfd) ] Add convenient method as helpers and use `PartialOrd` and `Ord` derive instructions [`Florentin Dubois`] (`2022-08-08`)
- [ [`4ccd277`](https://github.com/sozu-proxy/sozu/commit/4ccd27711368db4e163834bdf5424ab0c4a3eb02) ] rename "application" to "cluster" for consistency [`Emmanuel Bosquet`] (`2022-08-08`)
- [ [`8860c94`](https://github.com/sozu-proxy/sozu/commit/8860c945fdffba2fd2040f29fd5a1cd5460e1424) ] rudimentary lexicon [`Emmanuel Bosquet`] (`2022-08-09`)
- [ [`431b63f`](https://github.com/sozu-proxy/sozu/commit/431b63fa6b29b79a2f516eb626c80a2aa23bc439) ] debug a few things [`Emmanuel Bosquet`] (`2022-08-09`)
- [ [`9f029ac`](https://github.com/sozu-proxy/sozu/commit/9f029acfefac321db2febe5f7a1faeedc7370132) ] restore anyhow to 1.0.59 [`Emmanuel Bosquet`] (`2022-08-09`)
- [ [`0b9b92f`](https://github.com/sozu-proxy/sozu/commit/0b9b92f2ab2af6b6a70c527f506ccb3e24747a58) ] add an all-metrics command line option [`Emmanuel Bosquet`] (`2022-08-12`)
- [ [`9b7fdc9`](https://github.com/sozu-proxy/sozu/commit/9b7fdc9eed1f15572521e6d380dc442069141c10) ] tree_mut getter function, comments on struct fields, variable renaming [`Emmanuel Bosquet`] (`2022-08-16`)
- [ [`0cac89c`](https://github.com/sozu-proxy/sozu/commit/0cac89ce9fe9d912851c39e27407bd272d9049b7) ] refactor metrics printing with segreggated functions [`Emmanuel Bosquet`] (`2022-08-16`)
- [ [`4962f38`](https://github.com/sozu-proxy/sozu/commit/4962f3818033a1c53b931873d6cbf11a82383b26) ] list both proxy metric names and cluster metric names, refactoring, variable renaming [`Emmanuel Bosquet`] (`2022-08-17`)
- [ [`c8a9918`](https://github.com/sozu-proxy/sozu/commit/c8a9918801b518c4faafdfae0e8c6a0da206049b) ] metrics table formatting in the cli [`Emmanuel Bosquet`] (`2022-08-17`)
- [ [`1d6722e`](https://github.com/sozu-proxy/sozu/commit/1d6722e420c7e8de7f8063e1379db7e279b1a7b0) ] format metrics in nice boxes, suggestions in comments [`Emmanuel Bosquet`] (`2022-08-18`)
- [ [`ecf0353`](https://github.com/sozu-proxy/sozu/commit/ecf0353aec995bdbb024408ca75e13f13be73110) ] anyhow version to 1.0.62 [`Emmanuel Bosquet`] (`2022-08-18`)
- [ [`7b213e5`](https://github.com/sozu-proxy/sozu/commit/7b213e5338507b4292f4a15452ba4235c1a76dbf) ] trickle up errors if no metric for a backend or cluster [`Emmanuel Bosquet`] (`2022-08-23`)
- [ [`c44db1f`](https://github.com/sozu-proxy/sozu/commit/c44db1f9e62a914facebea743c94f0e3c43b79ba) ] Documenting comments and minor refactor [`Eloi DEMOLIS`] (`2022-08-31`)
- [ [`9b19aae`](https://github.com/sozu-proxy/sozu/commit/9b19aae8abd8b56e42a2b8dc1fcce86416116db1) ] Renamed RequestLine and StatusLine and their raw versions [`Eloi DEMOLIS`] (`2022-08-31`)
- [ [`8f2f1c0`](https://github.com/sozu-proxy/sozu/commit/8f2f1c08a6c1d45b8d669d63768584d6a6056de5) ] comments in bin imports, functions and struct fields,  refactoring [`Emmanuel Bosquet`] (`2022-09-02`)
- [ [`1a22d09`](https://github.com/sozu-proxy/sozu/commit/1a22d09f7be892cf6c3d5f9cdf8f38585284292d) ] proper error management on receive_listeners method of scm sockets [`Emmanuel Bosquet`] (`2022-09-02`)
- [ [`a30eb4e`](https://github.com/sozu-proxy/sozu/commit/a30eb4ee4401d8ee6c65f8786c44a8b5b04978ac) ] variable renaming, documenting comments, light refactoring [`Emmanuel Bosquet`] (`2022-09-02`)
- [ [`0955dcd`](https://github.com/sozu-proxy/sozu/commit/0955dcd97c6115484095c3f31be7ab39fe51676b) ] rename fields to better detailed names [`Emmanuel Bosquet`] (`2022-09-02`)
- [ [`0d88159`](https://github.com/sozu-proxy/sozu/commit/0d88159980e9e80f350e3be1ff7100ee65af2bc7) ] rename start_worker_process into fork_main_into_worker [`Emmanuel Bosquet`] (`2022-09-05`)
- [ [`5190f55`](https://github.com/sozu-proxy/sozu/commit/5190f555d5dce33f44241111b3b8a38471434b00) ] rename IPC sockets explicitly [`Emmanuel Bosquet`] (`2022-09-05`)
- [ [`74b82ce`](https://github.com/sozu-proxy/sozu/commit/74b82ce7c4a4193d929605749046ce278fd9b649) ] better naming for worker variables [`Emmanuel Bosquet`] (`2022-09-05`)
- [ [`5e77738`](https://github.com/sozu-proxy/sozu/commit/5e77738d723bd798ec96e3231698929a45c871a3) ] Renaming variables for clarity, light refactoring [`Eloi DEMOLIS`] (`2022-09-05`)
- [ [`7b30066`](https://github.com/sozu-proxy/sozu/commit/7b30066108fa8d63f02878a4656448844021c253) ] Potential place for 499 integration [`Eloi DEMOLIS`] (`2022-09-07`)
- [ [`3607dea`](https://github.com/sozu-proxy/sozu/commit/3607dea20f0be79da06bc594a00eaa867f0a2987) ] status() function in the Command Server (not finished) [`Emmanuel Bosquet`] (`2022-09-09`)
- [ [`6d293c5`](https://github.com/sozu-proxy/sozu/commit/6d293c5b29aad2a535123d2b37adc64a69ec5add) ] finished implementing displaying of worker statuses [`Emmanuel Bosquet`] (`2022-09-09`)
- [ [`b3c4f90`](https://github.com/sozu-proxy/sozu/commit/b3c4f90dc69b11368072b20ada9971c1d0a8214e) ] debugging [`Emmanuel Bosquet`] (`2022-09-12`)
- [ [`c79af23`](https://github.com/sozu-proxy/sozu/commit/c79af2399356ed48b9c2f7785070184976dc603e) ] delete legacy status function in ctl [`Emmanuel Bosquet`] (`2022-09-12`)
- [ [`8c1a0f4`](https://github.com/sozu-proxy/sozu/commit/8c1a0f41195405945b59bdf711ea15cc34d2bfff) ] anyhow error management on FileClusterConfig::to_cluster_config() and downstream [`Emmanuel Bosquet`] (`2022-09-19`)
- [ [`766f56c`](https://github.com/sozu-proxy/sozu/commit/766f56ca9304a3603fd70164eaa32c50d6cef41e) ] anyhow error management on FileConfig::into() and downstream [`Emmanuel Bosquet`] (`2022-09-21`)
- [ [`05a62f4`](https://github.com/sozu-proxy/sozu/commit/05a62f4a1d4c2c0f8656c0110bef17eda83b4d46) ] trickle errors on metrics setup [`Emmanuel Bosquet`] (`2022-09-21`)
- [ [`3ec6eb1`](https://github.com/sozu-proxy/sozu/commit/3ec6eb111801144b077d36c218008ac9f0d830e1) ] http::start() returns anyhow::Result, trickle errors [`Emmanuel Bosquet`] (`2022-09-22`)
- [ [`2563427`](https://github.com/sozu-proxy/sozu/commit/2563427e0b799a287af8db9c7cc94810a53a9e09) ] https_openssl::start() returns anyhow::Result, trickle errors [`Emmanuel Bosquet`] (`2022-09-22`)
- [ [`863b3cf`](https://github.com/sozu-proxy/sozu/commit/863b3cf2af4ff3685417c5428e70983fd1f4e7ce) ] error management in all server starts and Server::new() [`Emmanuel Bosquet`] (`2022-09-23`)
- [ [`abb4a7a`](https://github.com/sozu-proxy/sozu/commit/abb4a7a26502c87e97bf3f9c072dae6111c8e77d) ] Addresses part of #808 and #810 [`Eloi DEMOLIS`] (`2022-10-03`)
- [ [`53be70e`](https://github.com/sozu-proxy/sozu/commit/53be70e04228c904c8b18c611e19b8affb6c44e9) ] Fix HTTP crash for missing listeners on accept while shutdown. [`Eloi DEMOLIS`] (`2022-10-04`)
- [ [`1610b33`](https://github.com/sozu-proxy/sozu/commit/1610b3392e588d4b4ab560f0030b081e2851d0e9) ] Update dependencies and apply linter suggestions [`Florentin Dubois`] (`2022-10-04`)

#### ➖ Removed

- [ [`e82588a`](https://github.com/sozu-proxy/sozu/commit/e82588a15e2e207ecd705faea31aa8179de0d9f6) ] remove the end keys [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`fcc5d17`](https://github.com/sozu-proxy/sozu/commit/fcc5d17b133afa9fa86fbefd076a90e5aa744342) ] unused file [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`ce0b23c`](https://github.com/sozu-proxy/sozu/commit/ce0b23c657a49e3daf96a921f68ab245201ce4fc) ] remove log message [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5c808eb`](https://github.com/sozu-proxy/sozu/commit/5c808eb963658db8d285769a963b9b8cd1ca9a26) ] remove unused dependencies [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5ef9338`](https://github.com/sozu-proxy/sozu/commit/5ef9338a18bd9a80cd4a9ba57d3842e1c20740e6) ] Remove unused futures member [`Florentin Dubois`] (`2022-07-13`)
- [ [`4c7e8cb`](https://github.com/sozu-proxy/sozu/commit/4c7e8cbe734ebfab0594ee6c0d710229b1b81a5f) ] remove unwrap() and expect() statements [`Emmanuel Bosquet`] (`2022-09-19`)

#### ⛑️ Fixed

- [ [`05341d7`](https://github.com/sozu-proxy/sozu/commit/05341d76911aa6101447fe5fc683c3967390bcbf) ] fix: when targeting musl, construct msghdr differently [`Nathaniel`] (`2021-02-22`)
- [ [`f9af65e`](https://github.com/sozu-proxy/sozu/commit/f9af65eba32bdec5fb9d7f26484e3c31a9c05078) ] fix: link [`Gaël Reyrol`] (`2021-04-16`)
- [ [`931e198`](https://github.com/sozu-proxy/sozu/commit/931e19877a0494d2a0c898600b7ccaa4d49bfc97) ] fix: typo [`Gaël Reyrol`] (`2021-04-16`)
- [ [`6d2fcd0`](https://github.com/sozu-proxy/sozu/commit/6d2fcd045a5d93a531cf42b3de95d6c445ae6ce4) ] fix: typo [`Gaël Reyrol`] (`2021-04-16`)
- [ [`6981f09`](https://github.com/sozu-proxy/sozu/commit/6981f097ef423212469c8c8f63ba1d8e00d7e906) ] test (and fix) close delimited responses [`Geoffroy Couprie`] (`2021-08-20`)
- [ [`532a658`](https://github.com/sozu-proxy/sozu/commit/532a6589ae0b956aa4716a0cc0eba91aed719037) ] fix warnings [`Geoffroy Couprie`] (`2021-08-20`)
- [ [`bab0156`](https://github.com/sozu-proxy/sozu/commit/bab01562dd872876e7d181fbcb0900ee01b87c1a) ] fix missing slash [`Alexey Pozdnyakov`] (`2021-12-18`)
- [ [`d935da5`](https://github.com/sozu-proxy/sozu/commit/d935da5241b4ea243d5a8da8bc4b68b877579dc9) ] Allow passing domain names from the command line (#757) [`Sojan James`] (`2022-02-05`)
- [ [`e0700fb`](https://github.com/sozu-proxy/sozu/commit/e0700fbc4fa859b9ba00a3b803403a729f99cddf) ] Fix the config.toml file (#754) [`Hubert Bonisseur`] (`2022-02-10`)
- [ [`cbca349`](https://github.com/sozu-proxy/sozu/commit/cbca349441fa9aeb360a33fbcdc8c8a15815b838) ] fix build on stable [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`108876b`](https://github.com/sozu-proxy/sozu/commit/108876be561b3dbba4c2f1abf4e6c492b6f0572b) ] handle empty prefixes [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`ba6019e`](https://github.com/sozu-proxy/sozu/commit/ba6019edf213a5fe220a8a76c44f38ac083c3179) ] clean some warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`f5f6700`](https://github.com/sozu-proxy/sozu/commit/f5f6700f27e7c8a77393a236db6a67620d2c32fe) ] fix dependencies and compilation [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`5729d5f`](https://github.com/sozu-proxy/sozu/commit/5729d5f43e120806499296c9dc9d48e761bc04d4) ] fix state hashing [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`9b345af`](https://github.com/sozu-proxy/sozu/commit/9b345afc36dd1b4df57ea91994a984079373adcc) ] fix performance of state hashing [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`99b1d29`](https://github.com/sozu-proxy/sozu/commit/99b1d2961de6ac757dc35a7c1c0b4eb0ed28e04a) ] remove some warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`397620b`](https://github.com/sozu-proxy/sozu/commit/397620b9a41c3f7503b166cf82b67fb98269d490) ] missing RemoveCluster implementation [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`fce54ca`](https://github.com/sozu-proxy/sozu/commit/fce54ca083131538116a5defe45d513eaf181b92) ] fix unit tests [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`4b50212`](https://github.com/sozu-proxy/sozu/commit/4b50212a1b8aa0128c878e79802cd0cde9bfec2a) ] do not panic on sled errors [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`97e2fdd`](https://github.com/sozu-proxy/sozu/commit/97e2fdd38f973e75b49ea2d721255bf3c11830d0) ] fix answer message counting [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`d66ff8f`](https://github.com/sozu-proxy/sozu/commit/d66ff8fd201e02cd6f797f98065e0671a137de5e) ] fix some debug logs [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`478c6bb`](https://github.com/sozu-proxy/sozu/commit/478c6bbf2864d74ec428a72db4d3aac7da6d48ec) ] fix test compilation [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`e25013f`](https://github.com/sozu-proxy/sozu/commit/e25013fce55328c5da8c6866d65faa28cd597c88) ] fix warnings [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`4e17f86`](https://github.com/sozu-proxy/sozu/commit/4e17f86bffc58914d28cdf1a3fd39e6c0c51d742) ] fix doc build [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`4806fa1`](https://github.com/sozu-proxy/sozu/commit/4806fa1ea1ff93a3d338c1722b9cfd24b36e5ba6) ] fix the rustls proxy [`Geoffroy Couprie`] (`2022-07-13`)
- [ [`6c70039`](https://github.com/sozu-proxy/sozu/commit/6c7003942845c4ad0afe8208a86a49eff6db934d) ] fix a warning [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`01ec16c`](https://github.com/sozu-proxy/sozu/commit/01ec16c12101349b32b1caa6ec714ff6f2fd6545) ] fix socket removal on start [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`5a62a49`](https://github.com/sozu-proxy/sozu/commit/5a62a4969c9a761e14d8677dc4292494a2a17de9) ] safeguard against thread panick in edge case scenario [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`dc3edac`](https://github.com/sozu-proxy/sozu/commit/dc3edac8225ec72365989e5119b0b27ff894a829) ] check if a worker exists before trying to upgrade it [`Emmanuel Bosquet`] (`2022-07-13`)
- [ [`e5f9b1d`](https://github.com/sozu-proxy/sozu/commit/e5f9b1d85affc4861b7908ae42a19710bfe49c62) ] Fix main process upgrade and shutdown [`Eloi DEMOLIS`] (`2022-09-05`)
- [ [`f423a3e`](https://github.com/sozu-proxy/sozu/commit/f423a3e16e1a4dac613eaa3eea3fc08ea3e11c7c) ] Update parser of header value to trim linear white space [`Florentin Dubois`] (`2022-07-13`)

### 🥹 Contributors
* @arnolem made their first contribution in https://github.com/sozu-proxy/sozu/pull/727
* @av-elier made their first contribution in https://github.com/sozu-proxy/sozu/pull/746
* @sjames made their first contribution in https://github.com/sozu-proxy/sozu/pull/757
* @DeLaBatth made their first contribution in https://github.com/sozu-proxy/sozu/pull/754
* @LupusMichaelis made their first contribution in https://github.com/sozu-proxy/sozu/pull/755
* @Wonshtrum made their first contribution in https://github.com/sozu-proxy/sozu/pull/797
* @Geal
* @Keksoj
* @FlorentinDUBOIS


**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.13.6...v0.14.0

## 0.11.17 - 2019-07-24

debug release

### Fixed

- TLS 1.3 metric

### Changed

- removed domain fronting check (temporary)

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
