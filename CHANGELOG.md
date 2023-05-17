# Changelog

## [Unreleased]

See milestone [`v0.15.0`](https://github.com/sozu-proxy/sozu/projects/3?card_filter_query=milestone%3Av0.15.0)

## 0.14.3 - 2023-05-17

### 🌟 Features

- We have updated structures that Sōzu use for its communication on the socket. It now uses structures that are generated from protobuf. see [`b6bc86d`](https://github.com/sozu-proxy/sozu/commit/b6bc86ddf38987430cfd5fbb5c55b977f26ef861), [`2f4f769`](https://github.com/sozu-proxy/sozu/commit/2f4f7691d4efb193663e1e61999ca8478207009a), [`1732b5d`](https://github.com/sozu-proxy/sozu/commit/1732b5d1c1daf74e31704520a0f0330ca080d327), [`e3c8bec`](https://github.com/sozu-proxy/sozu/commit/e3c8beca54be48b1022ebdb5e5f9b9e671a4abbc), [`efb9c5d`](https://github.com/sozu-proxy/sozu/commit/efb9c5db6f28a6ecbd85a141fff7f68c6733dda9), [`7759984`](https://github.com/sozu-proxy/sozu/commit/77599849ca66897cd68e27b1fa0b66d735ae104c), [`1d5c72e`](https://github.com/sozu-proxy/sozu/commit/1d5c72e047fc108120eb9e5b08dd25c4692c9f89), [`1b07534`](https://github.com/sozu-proxy/sozu/commit/1b0753485af9ad8c3b7517f563504e3e0cd34bac), [`a1a801e`](https://github.com/sozu-proxy/sozu/commit/a1a801e29b9fdd1ad3724a991a24c45825a462f6), [`9dc490f`](https://github.com/sozu-proxy/sozu/commit/9dc490f27f3b4052581c8245cff97efd5946caa5), [`4bd9c6f`](https://github.com/sozu-proxy/sozu/commit/4bd9c6f8082b26d15f01e65983625cf9c43e14c5), [`1421f6c`](https://github.com/sozu-proxy/sozu/commit/1421f6ccf86caf49faefa58bcdf0fcb5b9b3c119), [`a39d905`](https://github.com/sozu-proxy/sozu/commit/a39d905ce9a9ca5407c8bae03f95acdb143e6032), [`43dfd6e`](https://github.com/sozu-proxy/sozu/commit/43dfd6e2d204fc52f9be232e72da9af60b734a52), [`6437a69`](https://github.com/sozu-proxy/sozu/commit/6437a69db97f9d6119754375b82cfcd76badd791), [`4ec1b21`](https://github.com/sozu-proxy/sozu/commit/4ec1b21e64a35a9c8f61a10a6d73ab08a71178d8), [`c4dbf90`](https://github.com/sozu-proxy/sozu/commit/c4dbf90bb9c372b3a98f611bc7a9b01fb84d3367), [`6910aaf`](https://github.com/sozu-proxy/sozu/commit/6910aaf28833584139addc2f8133ffccfa5c4481), [`137ae7f`](https://github.com/sozu-proxy/sozu/commit/137ae7fb0403ebff13fb8bbe187e74375d84311d), [`aeb2f2e`](https://github.com/sozu-proxy/sozu/commit/aeb2f2ea7601e5891186d41b469269ee90f5a5e7), [`7d9b0f5`](https://github.com/sozu-proxy/sozu/commit/7d9b0f5ce4c0c7b6a5c645f0ac2a5968719e58a3), [`755716c`](https://github.com/sozu-proxy/sozu/commit/755716c3aae451c47d3dd81b720a1e96abfa52d1), [`b2e0a6b`](https://github.com/sozu-proxy/sozu/commit/b2e0a6bc7b5ccb1a4bd0f4d496ca46abd9d4607e), [`4509bbe`](https://github.com/sozu-proxy/sozu/commit/4509bbea548ecce56fdf2a8056d6b9d88bd98717), [`c1f5b6e`](https://github.com/sozu-proxy/sozu/commit/c1f5b6eaf6d7391bf0db315cedcd83b707859e7d), [`2516e3b`](https://github.com/sozu-proxy/sozu/commit/2516e3bbb71b9ab61d5727c353b0c98706e7c0e0), [`d909f7d`](https://github.com/sozu-proxy/sozu/commit/d909f7d845d2ac201082efd1313273a58d8169eb), [`d2303f9`](https://github.com/sozu-proxy/sozu/commit/d2303f9fad0905c2c69172dc4e551540b24c42a7), [`8db54c7`](https://github.com/sozu-proxy/sozu/commit/8db54c725257fe79600f3f6120ff74c19f7f2fea), [`cbca836`](https://github.com/sozu-proxy/sozu/commit/cbca836d5331b0fe73521c40e02315f9259a6a96), [`35a43ee`](https://github.com/sozu-proxy/sozu/commit/35a43ee824d2c49f6bea40f978ebba124b83763c), [`2c6641a`](https://github.com/sozu-proxy/sozu/commit/2c6641a122779ff672889727e98d15d016849e60), [`7c6c93f`](https://github.com/sozu-proxy/sozu/commit/7c6c93fedc3f23b0cb37c2ba85eeb2b5e5d8e957), [`c53e763`](https://github.com/sozu-proxy/sozu/commit/c53e7639065698ba1d84b426e97205fe153c6832), [`e8fdb95`](https://github.com/sozu-proxy/sozu/commit/e8fdb95138035d164b425a13fb7be677f00c4dcc), [`c504ffa`](https://github.com/sozu-proxy/sozu/commit/c504ffacee6217af1bf11da896688e64bc8eb5da), [`0b74c92`](https://github.com/sozu-proxy/sozu/commit/0b74c9238e2d3340ad2b4e2859e2a94483d86064), [`7145d1f`](https://github.com/sozu-proxy/sozu/commit/7145d1f35d4542daecc70081e1774a9bbc167e5a), [`d6fba00`](https://github.com/sozu-proxy/sozu/commit/d6fba0087acadfa85c0eba69426414dfa50e52b5), [`ee301d9`](https://github.com/sozu-proxy/sozu/commit/ee301d961cbf5bfd8b82d79963a6d4d29c505bc8), [`8fcf5e9`](https://github.com/sozu-proxy/sozu/commit/8fcf5e9ede2b804589e1aaece25241e121344d96), [`f514bce`](https://github.com/sozu-proxy/sozu/commit/f514bce5b2a654d4f97c25826d15bbb69c4e1301), [`145d061`](https://github.com/sozu-proxy/sozu/commit/145d06186f1bc97831c02f698f5e471b69bdc526), [`81f4f39`](https://github.com/sozu-proxy/sozu/commit/81f4f39b275afcb6d376844d6f6827968c849661), [`7cf64c8`](https://github.com/sozu-proxy/sozu/commit/7cf64c8b2ce05306dab4e19c47946dfec1b04456), [`a7e16f8`](https://github.com/sozu-proxy/sozu/commit/a7e16f8eaca129e88ead09386416ed0491b623f6), [`4b312d4`](https://github.com/sozu-proxy/sozu/commit/4b312d45f54e8ba0417aa04f98ddae656a035a7b), [`fdb8f2e`](https://github.com/sozu-proxy/sozu/commit/fdb8f2ed0828b1da1f04023aa715d1307dfcef03), [`0e62f8d`](https://github.com/sozu-proxy/sozu/commit/0e62f8d8e2c38fbe9acfc663f87c775e500ea07a), [`e0643f4`](https://github.com/sozu-proxy/sozu/commit/e0643f492bb1b9f6d64db0adbc2ead18befe3c17), [`4df5ba9`](https://github.com/sozu-proxy/sozu/commit/4df5ba900802647039bbed6606d5ef4ec7c45c64), [`0c94c2b`](https://github.com/sozu-proxy/sozu/commit/0c94c2b34638f3ef558dbae7ad9af702f9c805f6), [`ad7f58f`](https://github.com/sozu-proxy/sozu/commit/ad7f58fa137b687923f5f0924c84970af85ad2a7), [`a5b0009`](https://github.com/sozu-proxy/sozu/commit/a5b0009cc978662660cb83ba9a743f2d3fd49e88), [`5f18b71`](https://github.com/sozu-proxy/sozu/commit/5f18b71fbd04981f8ab31641d00a45c762b77e18), [`cb4755a`](https://github.com/sozu-proxy/sozu/commit/cb4755a391ba5f9db74d2edc58d0130f741a7a55), [`eb9b18e`](https://github.com/sozu-proxy/sozu/commit/eb9b18e0c479dbfcecd41da2e7c9765a4a82ab76), [`3387628`](https://github.com/sozu-proxy/sozu/commit/3387628af71d539a887d3672212aa718492601f5), [`30ae223`](https://github.com/sozu-proxy/sozu/commit/30ae2238715a152a1612b5f8bc9485208184f2ba), [`72259e7`](https://github.com/sozu-proxy/sozu/commit/72259e76ff2698e4b51ea899c23dd93e059004b8), [`476e230`](https://github.com/sozu-proxy/sozu/commit/476e23043f715d32a1403ddad736af7cf087f641), [`7fac13a`](https://github.com/sozu-proxy/sozu/commit/7fac13a2a58f1dcfca1b9aed5dd05b878e820426), [`31d689a`](https://github.com/sozu-proxy/sozu/commit/31d689a0dbbcba5cc45c222c80a6e5b233809853), [`f03aac9`](https://github.com/sozu-proxy/sozu/commit/f03aac9f28dd89f800c2dc329bad97718547eb2b), [`5924101`](https://github.com/sozu-proxy/sozu/commit/5924101a12582d3b74dd0c116c13bb416062581c), [`8d6eb42`](https://github.com/sozu-proxy/sozu/commit/8d6eb42c50cc09ca35a9fde04a5ca30e673201c3), [`8e5e9ce`](https://github.com/sozu-proxy/sozu/commit/8e5e9ceea91459cf1bf856865642f73897ca228d), [`65b01e8`](https://github.com/sozu-proxy/sozu/commit/65b01e8893f72161007bf6a0862088c46a5fab17), [`1264378`](https://github.com/sozu-proxy/sozu/commit/1264378b5d5360bbdbab3268a317f39a78de5ad5), [`565dbd6`](https://github.com/sozu-proxy/sozu/commit/565dbd6ff559894384f508f434604e3ed42c50d4), [`87b4d96`](https://github.com/sozu-proxy/sozu/commit/87b4d96a83e6304bb817b32257b59b8396444ab7), [`457a9cd`](https://github.com/sozu-proxy/sozu/commit/457a9cd6fb0c3e00177722e65de34dc3b83d0c86), [`7ca33c6`](https://github.com/sozu-proxy/sozu/commit/7ca33c674d0d8c58e52ada5ce47476813a9b8433), [`2989f49`](https://github.com/sozu-proxy/sozu/commit/2989f49ecbdecbf864b1f0692e36a4f359544288), [`8907216`](https://github.com/sozu-proxy/sozu/commit/890721672d51bb464e3e1a6b0c2cad560ba1c060), [`36f01dc`](https://github.com/sozu-proxy/sozu/commit/36f01dc1e28facf5bb551180b3a6d2182890edbf), [`00f82cf`](https://github.com/sozu-proxy/sozu/commit/00f82cfeca758d921df7d28e82b8a491277048a1), [`9a37917`](https://github.com/sozu-proxy/sozu/commit/9a379176b7f4deeab6f7822283a24dba6efe92f6), [`5a77faf`](https://github.com/sozu-proxy/sozu/commit/5a77fafde2ca04ea17ddc84eaad05fed4b1c54c5), [`f434974`](https://github.com/sozu-proxy/sozu/commit/f4349748adbee1209d8d63189c74dc76285f1185), [`6716f8a`](https://github.com/sozu-proxy/sozu/commit/6716f8a06b4ff76e0e399ae7fd5a96d57c615086), [`4e1e7d0`](https://github.com/sozu-proxy/sozu/commit/4e1e7d0c0e9fe779cac55c22ba33a82bf204b5cf), [`25f6018`](https://github.com/sozu-proxy/sozu/commit/25f6018ffc2a23fbfca09f72d28ae6da7a3c4805), [`82bb6c5`](https://github.com/sozu-proxy/sozu/commit/82bb6c5b479640b90fe9fbdcd5727e1dbbc821ba), [`685bb16`](https://github.com/sozu-proxy/sozu/commit/685bb1630e39c3fcc59d2a26e5bf5907c169570a), [`bdd402f`](https://github.com/sozu-proxy/sozu/commit/bdd402f58ef9536186808fad913142cacb594a27), [`c3969d2`](https://github.com/sozu-proxy/sozu/commit/c3969d258d72c474eb7393e9fdb4c3ad9e1ca784), [`a87214f`](https://github.com/sozu-proxy/sozu/commit/a87214f8ddea6760cee313108f39ab7d866a2b31), [`ba6928c`](https://github.com/sozu-proxy/sozu/commit/ba6928ceb5d98c506418a46fb79e58716d5ef954), [`be75673`](https://github.com/sozu-proxy/sozu/commit/be75673c1c101a99ea9b7141429f543aa3db04b3), [`a8c87a1`](https://github.com/sozu-proxy/sozu/commit/a8c87a187ded0c1af8e97eb8b85dbea973686cff), [`c674b64`](https://github.com/sozu-proxy/sozu/commit/c674b64545546d5384920a8276e1497f13718769), [`f6a84e1`](https://github.com/sozu-proxy/sozu/commit/f6a84e1f09ce7595550c84dca8994d98c5cc2cae), [`03332c7`](https://github.com/sozu-proxy/sozu/commit/03332c71ae9c23bc9d12a5c76c39b62e1019f164), [`332ed3e`](https://github.com/sozu-proxy/sozu/commit/332ed3e6d0062683c5802ce3e0bee597bae7714a), [`1edcf68`](https://github.com/sozu-proxy/sozu/commit/1edcf6869e34e440d92234948c2a921ab4f1638e), [`3e41887`](https://github.com/sozu-proxy/sozu/commit/3e41887b9cebc6f23ecd1c1d9aecdb81791e4878), [`19d2915`](https://github.com/sozu-proxy/sozu/commit/19d29151cdecb1b0534efb5bc42b917c38782fa9), [`50b1a2e`](https://github.com/sozu-proxy/sozu/commit/50b1a2e5b62123e6b1864e2fb962b16f1eddd9fd), [`a91a576`](https://github.com/sozu-proxy/sozu/commit/a91a5765f9d7e6c48f4269996f4ae04d1e2e5826), [`6daea56`](https://github.com/sozu-proxy/sozu/commit/6daea56ab017c065ecdfbc5bdfaa5520d790a1fb), [`bd6d535`](https://github.com/sozu-proxy/sozu/commit/bd6d535c9174a34fb7e1714f2f53741788958c2e), [`7617b34`](https://github.com/sozu-proxy/sozu/commit/7617b345bf74e4fd7751f53bda99181c6fdd95d4), [`7254b98`](https://github.com/sozu-proxy/sozu/commit/7254b989cd859c97d70a13792fcf6e69ff2d10e1), [`c2df4b0`](https://github.com/sozu-proxy/sozu/commit/c2df4b06eb1377ea9f8611751333d47f29b938d3), [`d12920e`](https://github.com/sozu-proxy/sozu/commit/d12920e5d109c4cf42931a0bf8fdd105d1c2c864), [`4472332`](https://github.com/sozu-proxy/sozu/commit/4472332a2854382b492a7874795c5f7bed42c5ac), [`dbd1fad`](https://github.com/sozu-proxy/sozu/commit/dbd1fada9a24af882c12b677e4745a7c9b592fd1), [`66e36d6`](https://github.com/sozu-proxy/sozu/commit/66e36d6e00c501b3dedff9debdb2ea8d9dc195f0), [`582d285`](https://github.com/sozu-proxy/sozu/commit/582d2850679b8e57aed740aafaa6b553ae4993b1), [`d817d9d`](https://github.com/sozu-proxy/sozu/commit/d817d9d059b3c44180749c0e9841e414fe9ea874), [`7214aa2`](https://github.com/sozu-proxy/sozu/commit/7214aa2ade93f40171b2a4c0117482ab66279325), [`7fb08f3`](https://github.com/sozu-proxy/sozu/commit/7fb08f3a800369a08cfa639c740a263c9f5b10e7), [`97fb5da`](https://github.com/sozu-proxy/sozu/commit/97fb5dad7240be4125213960b26c79e222877ced), [`ae140f2`](https://github.com/sozu-proxy/sozu/commit/ae140f24a6fffd4a3192e5f9b79cfa5a21ed0401), [`2e26e76`](https://github.com/sozu-proxy/sozu/commit/2e26e763244214990915b8f7f36fad828338964f), [`30e602d`](https://github.com/sozu-proxy/sozu/commit/30e602ded83b11ac127bbe4ecd0e24efb59f99bd), [`f005d9a`](https://github.com/sozu-proxy/sozu/commit/f005d9a8ec5b8b1cfb1a9a54116470966d0f96e9), [`802c3e9`](https://github.com/sozu-proxy/sozu/commit/802c3e9aa1c55cf5770244fa13d877eb08ddefdf), [`75b5ade`](https://github.com/sozu-proxy/sozu/commit/75b5ade95b727d320f7f39ebc97fc09abdbc7743), [`537e0e9`](https://github.com/sozu-proxy/sozu/commit/537e0e9910b5641dac85f1aa5ed3dc4093fd2eaf), [`dbab4b4`](https://github.com/sozu-proxy/sozu/commit/dbab4b4c93a1a853641e69e035c6a82f6f10893c), [`8cebed1`](https://github.com/sozu-proxy/sozu/commit/8cebed1ba391507b3ed635bf13c7cb674ece1e66), [`73c496d`](https://github.com/sozu-proxy/sozu/commit/73c496dec0160126b370ba245f51e0b2a6765904), [`d51590c`](https://github.com/sozu-proxy/sozu/commit/d51590c139aea2bb784747b091c6a866c518b1ec), [`cdc8629`](https://github.com/sozu-proxy/sozu/commit/cdc8629201c9e9df51a72acdec9590e8baa8ec67), [`22719bd`](https://github.com/sozu-proxy/sozu/commit/22719bd14a57bbad77d37940ea96ec45c4dc4458), [`acb0455`](https://github.com/sozu-proxy/sozu/commit/acb0455235d08d679624cdb41526855f8fad6c61), [`f9de7ba`](https://github.com/sozu-proxy/sozu/commit/f9de7bab7f1dcdb30e88b5ffa67d28a86a565891), [`b125338`](https://github.com/sozu-proxy/sozu/commit/b125338a160bc8a91169ed10970230c1110b6681), [`edb6863`](https://github.com/sozu-proxy/sozu/commit/edb6863c9bdca8976c816b1c273cefb3ec60eab5) and [`570f8af`](https://github.com/sozu-proxy/sozu/commit/570f8af4486590bb7b3c77a9dd80630c505e5b8a).
- We have improved the command line internals, see [`c4c51fa`](https://github.com/sozu-proxy/sozu/commit/c4c51fa670654d3ee0d5918b87d4679f5391b077) and [`7b568dd`](https://github.com/sozu-proxy/sozu/commit/7b568dd92019216b756b56123dcb51225dc72f6b).
- We now publish a new docker image on each commit on main branch, see [`b198efb`](https://github.com/sozu-proxy/sozu/commit/b198efb3f1e774ddf64a345ec746a4d66fc45731) and [`b260c8c`](https://github.com/sozu-proxy/sozu/commit/b260c8c66b3926b272109cf4b1e8900155d1978c).
- We have set the minimum supported rust version to 1.66.1, see [`08504aa`](https://github.com/sozu-proxy/sozu/commit/08504aaf7cbf6045b053882e4ad4220f4d43cd83).

### ✍️ Changed

- We have implemented new tests on the e2e testing framework, see [`b5fb1d9`](https://github.com/sozu-proxy/sozu/commit/b5fb1d97dedbe8bbd3469af8f240fb31ccf6ce09).
- We have updated distribution packaging, see [`42fa3f2`](https://github.com/sozu-proxy/sozu/commit/42fa3f2afcb041f079bf2d93bc5d46afc5bffbb8), [`b7e2f17`](https://github.com/sozu-proxy/sozu/commit/b7e2f17d7213133a188d130b4d380503688f3466) and [`e5465e4`](https://github.com/sozu-proxy/sozu/commit/e5465e48649fac131ab949a30b0d41513f2d645f).
- We now build the documentation using only the stable version of Rust, see [`95f7019`](https://github.com/sozu-proxy/sozu/commit/95f70191e8c053fce34b476f35b08f643640a4ba).
- We also improved the documentation, see [`2609746`](https://github.com/sozu-proxy/sozu/commit/2609746f3f8842c9c813afe70693bdf6a41b37f7), [`057eac4`](https://github.com/sozu-proxy/sozu/commit/057eac456dc7ca4f59a3d88f990fbe53bcd7da28), [`432b22b`](https://github.com/sozu-proxy/sozu/commit/432b22b55dae19e6889e897f8f506ff945586f7e), [`91dc44d`](https://github.com/sozu-proxy/sozu/commit/91dc44d4b87be4dc7ac899b4e8e4c35c745ebd7f), [`78a6363`](https://github.com/sozu-proxy/sozu/commit/78a636387e36b1d77b9ccecd6a5300bcf39ed235), [`3e93455`](https://github.com/sozu-proxy/sozu/commit/3e93455a4281fef82f95fede93194622c7ced6e4), [`5032973`](https://github.com/sozu-proxy/sozu/commit/503297330756dae8614d20ba03e99f3d693189d3) and [`17f7cb6`](https://github.com/sozu-proxy/sozu/commit/17f7cb621fb93fac11968b7fa4062081a1812965).

### ➖ Removed

- We have removed the "acme" sub command of sozu to help us to completely remove openssl dependency on OpenSSL, see [`d5297dd`](https://github.com/sozu-proxy/sozu/commit/d5297dd0d60b99416df40fb827f47827566ea0d3), [`106d3c8`](https://github.com/sozu-proxy/sozu/commit/106d3c8277ae25b6f3f09d3e5a7f81c07a6ae612) and the issue [#926](https://github.com/sozu-proxy/sozu/issues/926)

### ⚡ Breaking changes

- As we changed the communication format from json to protobuf, we could not keep the compatibility with elder version. However, as we used protobuf now, we will be able to support evolutions and changes without creating a breaking change.

### Changelog

#### ➕ Added

- [ [`f0704ef`](https://github.com/sozu-proxy/sozu/commit/f0704ef5cc162e892001010698ba2169e7e3b95c) ] Add default variables [`tazou`] (`2021-06-22`)
- [ [`6616b77`](https://github.com/sozu-proxy/sozu/commit/6616b77c2000e3c5bb9baa2749db3a8fd71b42be) ] add context to HTTP and HTTPS listener activation [`Emmanuel Bosquet`] (`2022-12-09`)
- [ [`2f4f769`](https://github.com/sozu-proxy/sozu/commit/2f4f7691d4efb193663e1e61999ca8478207009a) ] add protobuf to Dockerfile image [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`08504aa`](https://github.com/sozu-proxy/sozu/commit/08504aaf7cbf6045b053882e4ad4220f4d43cd83) ] add rust-version and rust-toolchain [`Emmanuel Bosquet`] (`2023-05-05`)
- [ [`b198efb`](https://github.com/sozu-proxy/sozu/commit/b198efb3f1e774ddf64a345ec746a4d66fc45731) ] Docker build and push to Docker Hub in the CI [`Emmanuel Bosquet`] (`2022-12-09`)
- [ [`b260c8c`](https://github.com/sozu-proxy/sozu/commit/b260c8c66b3926b272109cf4b1e8900155d1978c) ] push docker build to docker hub only when merging to main [`Emmanuel Bosquet`] (`2022-12-09`)
- [ [`b5fb1d9`](https://github.com/sozu-proxy/sozu/commit/b5fb1d97dedbe8bbd3469af8f240fb31ccf6ce09) ] Simple e2e testing framework, passthrough and corner case tests [`Eloi DEMOLIS`] (`2023-01-10`)
- [ [`146dd32`](https://github.com/sozu-proxy/sozu/commit/146dd3205b8eb662e1bdbbf0531a2ae7790ca7ff) ] make get_cluster_ids_by_domain a method of ConfigState [`Emmanuel Bosquet`] (`2023-05-04`)
- [ [`ebeabc4`](https://github.com/sozu-proxy/sozu/commit/ebeabc4bf1f852974167bfb506fd309ecd31e570) ] make get_certificate a method of ConfigState [`Emmanuel Bosquet`] (`2023-05-04`)
- [ [`c4c51fa`](https://github.com/sozu-proxy/sozu/commit/c4c51fa670654d3ee0d5918b87d4679f5391b077) ] list HTTP, HTTPS and TCP listeners in the CLI [`Emmanuel Bosquet`] (`2023-01-12`)
- [ [`7b568dd`](https://github.com/sozu-proxy/sozu/commit/7b568dd92019216b756b56123dcb51225dc72f6b) ] cli: simplify request sending, remove boilerplate [`Emmanuel Bosquet`] (`2023-01-16`)
- [ [`7e24d12`](https://github.com/sozu-proxy/sozu/commit/7e24d12de55bbc231101f18f571d48390c7b700b) ] PR feedback [`Tim Bart`] (`2022-12-18`)

#### ➖ Removed

- [ [`106d3c8`](https://github.com/sozu-proxy/sozu/commit/106d3c8277ae25b6f3f09d3e5a7f81c07a6ae612) ] chore(acme): remove acme command [`Florentin Dubois`] (`2023-05-04`)
- [ [`5bf36f5`](https://github.com/sozu-proxy/sozu/commit/5bf36f557ec642c4ff0eda5752277275d56008d8) ] remove todo macros in bin/scr/ctl/command.rs [`Emmanuel Bosquet`] (`2023-01-16`)

#### ✍️ Changed

- [ [`42fa3f2`](https://github.com/sozu-proxy/sozu/commit/42fa3f2afcb041f079bf2d93bc5d46afc5bffbb8) ] chore(archlinux): update packaging [`Florentin Dubois`] (`2023-01-23`)
- [ [`b7e2f17`](https://github.com/sozu-proxy/sozu/commit/b7e2f17d7213133a188d130b4d380503688f3466) ] chore(docker): update packaging [`Florentin Dubois`] (`2023-01-23`)
- [ [`e5465e4`](https://github.com/sozu-proxy/sozu/commit/e5465e48649fac131ab949a30b0d41513f2d645f) ] chore(rpm): update packaging [`Florentin Dubois`] (`2023-01-23`)
- [ [`6c87883`](https://github.com/sozu-proxy/sozu/commit/6c87883cc15c1d2e3c21a9a43657a40c7b3783b3) ] Update generate.sh [`tazou`] (`2021-06-22`)
- [ [`1304914`](https://github.com/sozu-proxy/sozu/commit/1304914fc57dbcdf700556447daf73d7fa90d256) ] Update command/src/proxy.rs [`Tim Bart`] (`2022-12-20`)
- [ [`3e48735`](https://github.com/sozu-proxy/sozu/commit/3e487355e648830cc7333e3d0face6e2e25ce97e) ] remove unsafe for get_executable_path on macOS [`Tim Bart`] (`2022-12-19`)
- [ [`95f7019`](https://github.com/sozu-proxy/sozu/commit/95f70191e8c053fce34b476f35b08f643640a4ba) ] Github CI: use stable toolchain to build the doc [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`5698e14`](https://github.com/sozu-proxy/sozu/commit/5698e14997f697ef8e5bc42d92a96b1d54e31ed6) ] chore(lib): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [ [`c7fd33d`](https://github.com/sozu-proxy/sozu/commit/c7fd33dfc133adf16ba3e2d60a1ac837374cf8a5) ] chore(command): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [ [`9ca3cda`](https://github.com/sozu-proxy/sozu/commit/9ca3cda303acb831cf4e4acb1b8ef8bd35da8723) ] chore(e2e): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [ [`4a89060`](https://github.com/sozu-proxy/sozu/commit/4a89060baae9b600590a4435e7b8860ece42554c) ] chore(bin): bump dependencies [`Florentin Dubois`] (`2023-05-03`)
- [ [`78f1647`](https://github.com/sozu-proxy/sozu/commit/78f16476f06ac95d353db292316de4d474b9ef15) ] update rustls to 0.21.0 [`Emmanuel Bosquet`] (`2023-04-12`)
- [ [`6d5215a`](https://github.com/sozu-proxy/sozu/commit/6d5215acafd99dea484b7c896126aa068e3c389b) ] Update dependencies [`Florentin Dubois`] (`2023-02-06`)
- [ [`b6bc86d`](https://github.com/sozu-proxy/sozu/commit/b6bc86ddf38987430cfd5fbb5c55b977f26ef861) ] build protobuf types with prost-build, without tonic [`Emmanuel Bosquet`] (`2023-04-12`)
- [ [`d5297dd`](https://github.com/sozu-proxy/sozu/commit/d5297dd0d60b99416df40fb827f47827566ea0d3) ] chore(e2e): use rustls instead of openssl [`Florentin Dubois`] (`2023-05-04`)
- [ [`b978a89`](https://github.com/sozu-proxy/sozu/commit/b978a891cffd72e467a071597d7ffc92428e5432) ] get TcpFrontends::tags out of its Option<> [`Emmanuel Bosquet`] (`2023-05-04`)
- [ [`9cd613c`](https://github.com/sozu-proxy/sozu/commit/9cd613c199b05e14e6f4525f851392391b38f722) ] Update lib/src/protocol/http/mod.rs [`Eloi Démolis`] (`2023-04-13`)
- [ [`b89f4f7`](https://github.com/sozu-proxy/sozu/commit/b89f4f7d72d6075f5661c2875cfedef43c15280f) ] Update lib/src/protocol/http/mod.rs [`Eloi Démolis`] (`2023-04-13`)
- [ [`a1c02b6`](https://github.com/sozu-proxy/sozu/commit/a1c02b6b76fc5a7254c86788e2d2572777fbeafd) ] remove resolved TODOs [`Emmanuel Bosquet`] (`2023-05-04`)

#### 📚 Documentation

- [ [`2609746`](https://github.com/sozu-proxy/sozu/commit/2609746f3f8842c9c813afe70693bdf6a41b37f7) ] add documenting comments [`Emmanuel Bosquet`] (`2022-12-16`)
- [ [`057eac4`](https://github.com/sozu-proxy/sozu/commit/057eac456dc7ca4f59a3d88f990fbe53bcd7da28) ] basic crate documentation on sozu [`Emmanuel Bosquet`] (`2023-05-09`)
- [ [`432b22b`](https://github.com/sozu-proxy/sozu/commit/432b22b55dae19e6889e897f8f506ff945586f7e) ] documenting comments on main process upgrade, remove useless comments [`Emmanuel Bosquet`] (`2022-12-16`)
- [ [`91dc44d`](https://github.com/sozu-proxy/sozu/commit/91dc44d4b87be4dc7ac899b4e8e4c35c745ebd7f) ] Remove mentions of sozuctl [`Sykursen`] (`2023-03-01`)
- [ [`78a6363`](https://github.com/sozu-proxy/sozu/commit/78a636387e36b1d77b9ccecd6a5300bcf39ed235) ] document protobuf in the sozu-command-lib README [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`3e93455`](https://github.com/sozu-proxy/sozu/commit/3e93455a4281fef82f95fede93194622c7ced6e4) ] doc: use sozu instead of sozuctl [`Florentin Dubois`] (`2023-01-23`)
- [ [`5032973`](https://github.com/sozu-proxy/sozu/commit/503297330756dae8614d20ba03e99f3d693189d3) ] correct formatting in how_to_use.md [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`17f7cb6`](https://github.com/sozu-proxy/sozu/commit/17f7cb621fb93fac11968b7fa4062081a1812965) ] rewrite the sozu_lib documentation [`Emmanuel Bosquet`] (`2023-05-09`)

#### 🚀 Refactored

- [ [`319119a`](https://github.com/sozu-proxy/sozu/commit/319119a46b155fafd6a01a63ce757dfa7d48d3e6) ] abstract out HTTP and HTTPS notify methods [`Emmanuel Bosquet`] (`2022-12-08`)
- [ [`03085ea`](https://github.com/sozu-proxy/sozu/commit/03085eab569e6208def948a3b65830c3cd3b9f27) ] error propagation on HTTP and HTTPS frontend add and remove [`Emmanuel Bosquet`] (`2022-12-08`)
- [ [`ffb5384`](https://github.com/sozu-proxy/sozu/commit/ffb5384c2454c4077f49456376b7ca68d5deb2c7) ] rename ConfigState::handle_order to ConfigState::dispatch [`Emmanuel Bosquet`] (`2022-12-16`)
- [ [`1c9f785`](https://github.com/sozu-proxy/sozu/commit/1c9f785fc5a8ee54be8f99ece08ed3913e415feb) ] remove the init_workers function [`Emmanuel Bosquet`] (`2022-12-16`)
- [ [`1732b5d`](https://github.com/sozu-proxy/sozu/commit/1732b5d1c1daf74e31704520a0f0330ca080d327) ] rename command::proxy module to command::worker [`Emmanuel Bosquet`] (`2023-03-08`)
- [ [`e3c8bec`](https://github.com/sozu-proxy/sozu/commit/e3c8beca54be48b1022ebdb5e5f9b9e671a4abbc) ] remove optional worker id from CommandRequest [`Emmanuel Bosquet`] (`2023-03-08`)
- [ [`efb9c5d`](https://github.com/sozu-proxy/sozu/commit/efb9c5db6f28a6ecbd85a141fff7f68c6733dda9) ] rename CommandRequest to ClientRequest [`Emmanuel Bosquet`] (`2023-03-08`)
- [ [`7759984`](https://github.com/sozu-proxy/sozu/commit/77599849ca66897cd68e27b1fa0b66d735ae104c) ] flatten ProxyRequestOrder variants into RequestContent [`Emmanuel Bosquet`] (`2023-03-08`)
- [ [`1d5c72e`](https://github.com/sozu-proxy/sozu/commit/1d5c72e047fc108120eb9e5b08dd25c4692c9f89) ] remove id and version from Requests sent to sozu [`Emmanuel Bosquet`] (`2023-03-09`)
- [ [`1b07534`](https://github.com/sozu-proxy/sozu/commit/1b0753485af9ad8c3b7517f563504e3e0cd34bac) ] Remove @BlackYoup from code owners [`Florentin DUBOIS`] (`2023-03-09`)
- [ [`a1a801e`](https://github.com/sozu-proxy/sozu/commit/a1a801e29b9fdd1ad3724a991a24c45825a462f6) ] put Query variants into Order, remove Query [`Emmanuel Bosquet`] (`2023-03-09`)
- [ [`9dc490f`](https://github.com/sozu-proxy/sozu/commit/9dc490f27f3b4052581c8245cff97efd5946caa5) ] sozu_command_lib: rename command module to order [`Emmanuel Bosquet`] (`2023-03-10`)
- [ [`4bd9c6f`](https://github.com/sozu-proxy/sozu/commit/4bd9c6f8082b26d15f01e65983625cf9c43e14c5) ] segregate types in the order and response modules [`Emmanuel Bosquet`] (`2023-03-10`)
- [ [`1421f6c`](https://github.com/sozu-proxy/sozu/commit/1421f6ccf86caf49faefa58bcdf0fcb5b9b3c119) ] rename sozu::Response to sozu::Advancement [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`a39d905`](https://github.com/sozu-proxy/sozu/commit/a39d905ce9a9ca5407c8bae03f95acdb143e6032) ] rename sozu_command_lib::CommandResponse to Response [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`43dfd6e`](https://github.com/sozu-proxy/sozu/commit/43dfd6e2d204fc52f9be232e72da9af60b734a52) ] rename sozu_command_lib::order module to request [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`6437a69`](https://github.com/sozu-proxy/sozu/commit/6437a69db97f9d6119754375b82cfcd76badd791) ] rename sozu::command::orders to sozu::command::requests [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`4ec1b21`](https://github.com/sozu-proxy/sozu/commit/4ec1b21e64a35a9c8f61a10a6d73ab08a71178d8) ] sozu::Worker::is_not_stopped_or_stopping() method [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`c4dbf90`](https://github.com/sozu-proxy/sozu/commit/c4dbf90bb9c372b3a98f611bc7a9b01fb84d3367) ] method Request::is_a_stop() [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`6910aaf`](https://github.com/sozu-proxy/sozu/commit/6910aaf28833584139addc2f8133ffccfa5c4481) ] remove worker.rs [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`137ae7f`](https://github.com/sozu-proxy/sozu/commit/137ae7fb0403ebff13fb8bbe187e74375d84311d) ] return error if no worker is found when reloading configuration [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`aeb2f2e`](https://github.com/sozu-proxy/sozu/commit/aeb2f2ea7601e5891186d41b469269ee90f5a5e7) ] remove useless ProxyEvent, redundant with Event [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`7d9b0f5`](https://github.com/sozu-proxy/sozu/commit/7d9b0f5ce4c0c7b6a5c645f0ac2a5968719e58a3) ] use type ResponseStatus for ProxyResponse [`Emmanuel Bosquet`] (`2023-03-13`)
- [ [`755716c`](https://github.com/sozu-proxy/sozu/commit/755716c3aae451c47d3dd81b720a1e96abfa52d1) ] rename sozu::Worker::is_not_stopped_or_stopping to is_active [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`b2e0a6b`](https://github.com/sozu-proxy/sozu/commit/b2e0a6bc7b5ccb1a4bd0f4d496ca46abd9d4607e) ] build Config using a ConfigBuilder [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`4509bbe`](https://github.com/sozu-proxy/sozu/commit/4509bbea548ecce56fdf2a8056d6b9d88bd98717) ] sozu_command_lib::config::FileConfig::load_from_path returns anyhow::Result [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`c1f5b6e`](https://github.com/sozu-proxy/sozu/commit/c1f5b6eaf6d7391bf0db315cedcd83b707859e7d) ] parse String to SocketAddr in config::Listener [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`2516e3b`](https://github.com/sozu-proxy/sozu/commit/2516e3bbb71b9ab61d5727c353b0c98706e7c0e0) ] replace SocketAddr with String in certificate requests [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`d909f7d`](https://github.com/sozu-proxy/sozu/commit/d909f7d845d2ac201082efd1313273a58d8169eb) ] create struct AddBackend where SocketAddr is a String [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`d2303f9`](https://github.com/sozu-proxy/sozu/commit/d2303f9fad0905c2c69172dc4e551540b24c42a7) ] create struct RequestHttpFrontend where SocketAddr is a String [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`8db54c7`](https://github.com/sozu-proxy/sozu/commit/8db54c725257fe79600f3f6120ff74c19f7f2fea) ] documenting comments and defaults on Config [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`cbca836`](https://github.com/sozu-proxy/sozu/commit/cbca836d5331b0fe73521c40e02315f9259a6a96) ] builder pattern for listeners [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`35a43ee`](https://github.com/sozu-proxy/sozu/commit/35a43ee824d2c49f6bea40f978ebba124b83763c) ] default values for timeouts in Config serialization [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`2c6641a`](https://github.com/sozu-proxy/sozu/commit/2c6641a122779ff672889727e98d15d016849e60) ] const DEFAULT_STICKY_NAME with value SOZUBALANCEID [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`7c6c93f`](https://github.com/sozu-proxy/sozu/commit/7c6c93fedc3f23b0cb37c2ba85eeb2b5e5d8e957) ] protocol checks when building Listener in config.rs [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`c53e763`](https://github.com/sozu-proxy/sozu/commit/c53e7639065698ba1d84b426e97205fe153c6832) ] documenting comments on listener builders [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`e8fdb95`](https://github.com/sozu-proxy/sozu/commit/e8fdb95138035d164b425a13fb7be677f00c4dcc) ] implement Into<RequestHttpFrontend> for HttpFrontend [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`c504ffa`](https://github.com/sozu-proxy/sozu/commit/c504ffacee6217af1bf11da896688e64bc8eb5da) ] remove impl Default for HttpListenerConfig [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`0b74c92`](https://github.com/sozu-proxy/sozu/commit/0b74c9238e2d3340ad2b4e2859e2a94483d86064) ] remove useless field http_addresses on ConfigState [`Emmanuel Bosquet`] (`2023-03-16`)
- [ [`7145d1f`](https://github.com/sozu-proxy/sozu/commit/7145d1f35d4542daecc70081e1774a9bbc167e5a) ] parse socket addresses in the CLI before sending requests [`Emmanuel Bosquet`] (`2023-03-20`)
- [ [`d6fba00`](https://github.com/sozu-proxy/sozu/commit/d6fba0087acadfa85c0eba69426414dfa50e52b5) ] rename QueryAnswerCluster to ClusterInformation [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`ee301d9`](https://github.com/sozu-proxy/sozu/commit/ee301d961cbf5bfd8b82d79963a6d4d29c505bc8) ] rename CertificateFingerprint to Fingerprint [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`8fcf5e9`](https://github.com/sozu-proxy/sozu/commit/8fcf5e9ede2b804589e1aaece25241e121344d96) ] introduce response type CertificateSummary [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`f514bce`](https://github.com/sozu-proxy/sozu/commit/f514bce5b2a654d4f97c25826d15bbb69c4e1301) ] create Request::QueryAllCertificates [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`145d061`](https://github.com/sozu-proxy/sozu/commit/145d06186f1bc97831c02f698f5e471b69bdc526) ] create Request::QueryCertificatesByDomain [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`81f4f39`](https://github.com/sozu-proxy/sozu/commit/81f4f39b275afcb6d376844d6f6827968c849661) ] create Request::QueryCertificateByFingerprint [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`7cf64c8`](https://github.com/sozu-proxy/sozu/commit/7cf64c8b2ce05306dab4e19c47946dfec1b04456) ] make Request::QueryCertificateByFingerprint contain Fingerprint [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`a7e16f8`](https://github.com/sozu-proxy/sozu/commit/a7e16f8eaca129e88ead09386416ed0491b623f6) ] remove ResponseContent::CertificatesByDomain [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`4b312d4`](https://github.com/sozu-proxy/sozu/commit/4b312d45f54e8ba0417aa04f98ddae656a035a7b) ] remove QueryAnswerCertificate [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`fdb8f2e`](https://github.com/sozu-proxy/sozu/commit/fdb8f2ed0828b1da1f04023aa715d1307dfcef03) ] rename ClusterMetricsData to ClusterMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`0e62f8d`](https://github.com/sozu-proxy/sozu/commit/0e62f8d8e2c38fbe9acfc663f87c775e500ea07a) ] remove ProxyResponseContent, put its variant in ResponseContent [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`e0643f4`](https://github.com/sozu-proxy/sozu/commit/e0643f492bb1b9f6d64db0adbc2ead18befe3c17) ] rename ProxyResponse to WorkerResponse [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`4df5ba9`](https://github.com/sozu-proxy/sozu/commit/4df5ba900802647039bbed6606d5ef4ec7c45c64) ] put QueryAnswer variants in ProxyResponse, remove QueryAnswer [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`0c94c2b`](https://github.com/sozu-proxy/sozu/commit/0c94c2b34638f3ef558dbae7ad9af702f9c805f6) ] make PathRule a struct, with embedded enum PathRuleKind [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`ad7f58f`](https://github.com/sozu-proxy/sozu/commit/ad7f58fa137b687923f5f0924c84970af85ad2a7) ] rename AggregatedMetricsData to AggregatedMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`a5b0009`](https://github.com/sozu-proxy/sozu/commit/a5b0009cc978662660cb83ba9a743f2d3fd49e88) ] rename FilteredData to FilteredMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`5f18b71`](https://github.com/sozu-proxy/sozu/commit/5f18b71fbd04981f8ab31641d00a45c762b77e18) ] create and use response::BackendMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`cb4755a`](https://github.com/sozu-proxy/sozu/commit/cb4755a391ba5f9db74d2edc58d0130f741a7a55) ] make AggregatedMetrics contain WorkerMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`eb9b18e`](https://github.com/sozu-proxy/sozu/commit/eb9b18e0c479dbfcecd41da2e7c9765a4a82ab76) ] create AvailableMetrics, remove QueryAnswerMetrics [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`3387628`](https://github.com/sozu-proxy/sozu/commit/3387628af71d539a887d3672212aa718492601f5) ] move QueryAnswerCertificate::All to ResponseContent::AllCertificates [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`30ae223`](https://github.com/sozu-proxy/sozu/commit/30ae2238715a152a1612b5f8bc9485208184f2ba) ] move QueryAnswerCertificate::Domain to ResponseContent::CertificatesByDomain [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`72259e7`](https://github.com/sozu-proxy/sozu/commit/72259e76ff2698e4b51ea899c23dd93e059004b8) ] move QueryAnswerCertificate::Fingerprint to ResponseContent::CertificateByFingerprint [`Emmanuel Bosquet`] (`2023-03-22`)
- [ [`476e230`](https://github.com/sozu-proxy/sozu/commit/476e23043f715d32a1403ddad736af7cf087f641) ] Http refactor: move backend logic from Session to State [`Eloi DEMOLIS`] (`2023-02-13`)
- [ [`7fac13a`](https://github.com/sozu-proxy/sozu/commit/7fac13a2a58f1dcfca1b9aed5dd05b878e820426) ] create Request::QueryClusterById [`Emmanuel Bosquet`] (`2023-03-23`)
- [ [`31d689a`](https://github.com/sozu-proxy/sozu/commit/31d689a0dbbcba5cc45c222c80a6e5b233809853) ] create Request::QueryClusterByDomain [`Emmanuel Bosquet`] (`2023-03-23`)
- [ [`f03aac9`](https://github.com/sozu-proxy/sozu/commit/f03aac9f28dd89f800c2dc329bad97718547eb2b) ] HttpFrontend::route has type Option<ClusterId> [`Emmanuel Bosquet`] (`2023-03-27`)
- [ [`5924101`](https://github.com/sozu-proxy/sozu/commit/5924101a12582d3b74dd0c116c13bb416062581c) ] rename HttpFrontend::route to cluster_id [`Emmanuel Bosquet`] (`2023-03-27`)
- [ [`8d6eb42`](https://github.com/sozu-proxy/sozu/commit/8d6eb42c50cc09ca35a9fde04a5ca30e673201c3) ] replace SocketAddr type with String in Listeners [`Emmanuel Bosquet`] (`2023-04-03`)
- [ [`8e5e9ce`](https://github.com/sozu-proxy/sozu/commit/8e5e9ceea91459cf1bf856865642f73897ca228d) ] field active on TcpListenerConfig [`Emmanuel Bosquet`] (`2023-04-03`)
- [ [`65b01e8`](https://github.com/sozu-proxy/sozu/commit/65b01e8893f72161007bf6a0862088c46a5fab17) ] field active on HttpsListenerConfig [`Emmanuel Bosquet`] (`2023-04-03`)
- [ [`1264378`](https://github.com/sozu-proxy/sozu/commit/1264378b5d5360bbdbab3268a317f39a78de5ad5) ] field active on HttpListenerConfig [`Emmanuel Bosquet`] (`2023-04-03`)
- [ [`565dbd6`](https://github.com/sozu-proxy/sozu/commit/565dbd6ff559894384f508f434604e3ed42c50d4) ] implement fmt::Display for RequestHttpFrontend [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`87b4d96`](https://github.com/sozu-proxy/sozu/commit/87b4d96a83e6304bb817b32257b59b8396444ab7) ] populate https_frontends in ConfigState [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`457a9cd`](https://github.com/sozu-proxy/sozu/commit/457a9cd6fb0c3e00177722e65de34dc3b83d0c86) ] protobuf scaffold [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`7ca33c6`](https://github.com/sozu-proxy/sozu/commit/7ca33c674d0d8c58e52ada5ce47476813a9b8433) ] write RequestHttpFrontend in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`2989f49`](https://github.com/sozu-proxy/sozu/commit/2989f49ecbdecbf864b1f0692e36a4f359544288) ] write CertificateSummary in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`8907216`](https://github.com/sozu-proxy/sozu/commit/890721672d51bb464e3e1a6b0c2cad560ba1c060) ] write TlsVersion in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`36f01dc`](https://github.com/sozu-proxy/sozu/commit/36f01dc1e28facf5bb551180b3a6d2182890edbf) ] write CertificateAndKey in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`00f82cf`](https://github.com/sozu-proxy/sozu/commit/00f82cfeca758d921df7d28e82b8a491277048a1) ] write AddCertificate in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`9a37917`](https://github.com/sozu-proxy/sozu/commit/9a379176b7f4deeab6f7822283a24dba6efe92f6) ] write RemoveCertificate in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`5a77faf`](https://github.com/sozu-proxy/sozu/commit/5a77fafde2ca04ea17ddc84eaad05fed4b1c54c5) ] write ReplaceCertificate in protobuf [`Emmanuel Bosquet`] (`2023-04-04`)
- [ [`f434974`](https://github.com/sozu-proxy/sozu/commit/f4349748adbee1209d8d63189c74dc76285f1185) ] write LoadBalancingAlgorithms in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`6716f8a`](https://github.com/sozu-proxy/sozu/commit/6716f8a06b4ff76e0e399ae7fd5a96d57c615086) ] write ProxyProtocolConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`4e1e7d0`](https://github.com/sozu-proxy/sozu/commit/4e1e7d0c0e9fe779cac55c22ba33a82bf204b5cf) ] write LoadMetric in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`25f6018`](https://github.com/sozu-proxy/sozu/commit/25f6018ffc2a23fbfca09f72d28ae6da7a3c4805) ] write Cluster in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`82bb6c5`](https://github.com/sozu-proxy/sozu/commit/82bb6c5b479640b90fe9fbdcd5727e1dbbc821ba) ] write RequestTcpFrontend in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`685bb16`](https://github.com/sozu-proxy/sozu/commit/685bb1630e39c3fcc59d2a26e5bf5907c169570a) ] write RemoveBackend in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`bdd402f`](https://github.com/sozu-proxy/sozu/commit/bdd402f58ef9536186808fad913142cacb594a27) ] write AddBackend and LoadBalancingParams in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`c3969d2`](https://github.com/sozu-proxy/sozu/commit/c3969d258d72c474eb7393e9fdb4c3ad9e1ca784) ] write QueryClusterByDomain in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`a87214f`](https://github.com/sozu-proxy/sozu/commit/a87214f8ddea6760cee313108f39ab7d866a2b31) ] write QueryMetricsOptions in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`ba6928c`](https://github.com/sozu-proxy/sozu/commit/ba6928ceb5d98c506418a46fb79e58716d5ef954) ] write MetricsConfiguration in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`be75673`](https://github.com/sozu-proxy/sozu/commit/be75673c1c101a99ea9b7141429f543aa3db04b3) ] write RunState in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`a8c87a1`](https://github.com/sozu-proxy/sozu/commit/a8c87a187ded0c1af8e97eb8b85dbea973686cff) ] write WorkerInfo in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`c674b64`](https://github.com/sozu-proxy/sozu/commit/c674b64545546d5384920a8276e1497f13718769) ] write Percentiles in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`f6a84e1`](https://github.com/sozu-proxy/sozu/commit/f6a84e1f09ce7595550c84dca8994d98c5cc2cae) ] write FilteredTimeSerie in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`03332c7`](https://github.com/sozu-proxy/sozu/commit/03332c71ae9c23bc9d12a5c76c39b62e1019f164) ] write FilteredMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`332ed3e`](https://github.com/sozu-proxy/sozu/commit/332ed3e6d0062683c5802ce3e0bee597bae7714a) ] write BackendMetrics and ClusterMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`1edcf68`](https://github.com/sozu-proxy/sozu/commit/1edcf6869e34e440d92234948c2a921ab4f1638e) ] write WorkerMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`3e41887`](https://github.com/sozu-proxy/sozu/commit/3e41887b9cebc6f23ecd1c1d9aecdb81791e4878) ] write AggregatedMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`19d2915`](https://github.com/sozu-proxy/sozu/commit/19d29151cdecb1b0534efb5bc42b917c38782fa9) ] put field names to CertificateAndKey [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`50b1a2e`](https://github.com/sozu-proxy/sozu/commit/50b1a2e5b62123e6b1864e2fb962b16f1eddd9fd) ] write HttpFrontendConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`a91a576`](https://github.com/sozu-proxy/sozu/commit/a91a5765f9d7e6c48f4269996f4ae04d1e2e5826) ] write HttpsFrontendConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`6daea56`](https://github.com/sozu-proxy/sozu/commit/6daea56ab017c065ecdfbc5bdfaa5520d790a1fb) ] write TcpListenerConfig in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`bd6d535`](https://github.com/sozu-proxy/sozu/commit/bd6d535c9174a34fb7e1714f2f53741788958c2e) ] write ListenersList in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`7617b34`](https://github.com/sozu-proxy/sozu/commit/7617b345bf74e4fd7751f53bda99181c6fdd95d4) ] write ListenerType in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`7254b98`](https://github.com/sozu-proxy/sozu/commit/7254b989cd859c97d70a13792fcf6e69ff2d10e1) ] write RemoveListener in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`c2df4b0`](https://github.com/sozu-proxy/sozu/commit/c2df4b06eb1377ea9f8611751333d47f29b938d3) ] write ActivateListener in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`d12920e`](https://github.com/sozu-proxy/sozu/commit/d12920e5d109c4cf42931a0bf8fdd105d1c2c864) ] write DeactivateListener in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`4472332`](https://github.com/sozu-proxy/sozu/commit/4472332a2854382b492a7874795c5f7bed42c5ac) ] remove useless HttpProxy, HttpsProxy, add TODOs [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`dbd1fad`](https://github.com/sozu-proxy/sozu/commit/dbd1fada9a24af882c12b677e4745a7c9b592fd1) ] write AvailableMetrics in protobuf [`Emmanuel Bosquet`] (`2023-04-05`)
- [ [`66e36d6`](https://github.com/sozu-proxy/sozu/commit/66e36d6e00c501b3dedff9debdb2ea8d9dc195f0) ] write Request in protobuf [`Emmanuel Bosquet`] (`2023-04-26`)
- [ [`582d285`](https://github.com/sozu-proxy/sozu/commit/582d2850679b8e57aed740aafaa6b553ae4993b1) ] write ResponseStatus in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`d817d9d`](https://github.com/sozu-proxy/sozu/commit/d817d9d059b3c44180749c0e9841e414fe9ea874) ] create type WorkerInfos [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`7214aa2`](https://github.com/sozu-proxy/sozu/commit/7214aa2ade93f40171b2a4c0117482ab66279325) ] replace ResponseContent::Status with ResponseContent::Workers [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`7fb08f3`](https://github.com/sozu-proxy/sozu/commit/7fb08f3a800369a08cfa639c740a263c9f5b10e7) ] make response::Event a struct, create EventKind [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`97fb5da`](https://github.com/sozu-proxy/sozu/commit/97fb5dad7240be4125213960b26c79e222877ced) ] write Event and EventKind in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`ae140f2`](https://github.com/sozu-proxy/sozu/commit/ae140f24a6fffd4a3192e5f9b79cfa5a21ed0401) ] remove the DumpState command [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`2e26e76`](https://github.com/sozu-proxy/sozu/commit/2e26e763244214990915b8f7f36fad828338964f) ] create type ClusterHashes [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`30e602d`](https://github.com/sozu-proxy/sozu/commit/30e602ded83b11ac127bbe4ecd0e24efb59f99bd) ] write ClusterInformation in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`f005d9a`](https://github.com/sozu-proxy/sozu/commit/f005d9a8ec5b8b1cfb1a9a54116470966d0f96e9) ] create type ClusterInformations [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`802c3e9`](https://github.com/sozu-proxy/sozu/commit/802c3e9aa1c55cf5770244fa13d877eb08ddefdf) ] create type CertificateWithNames [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`75b5ade`](https://github.com/sozu-proxy/sozu/commit/75b5ade95b727d320f7f39ebc97fc09abdbc7743) ] create types ListOfCertificatesByAddress and CertificatesByAddress [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`537e0e9`](https://github.com/sozu-proxy/sozu/commit/537e0e9910b5641dac85f1aa5ed3dc4093fd2eaf) ] write ListedFrontends in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`dbab4b4`](https://github.com/sozu-proxy/sozu/commit/dbab4b4c93a1a853641e69e035c6a82f6f10893c) ] write ResponseContent in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`8cebed1`](https://github.com/sozu-proxy/sozu/commit/8cebed1ba391507b3ed635bf13c7cb674ece1e66) ] remove id from Response [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`73c496d`](https://github.com/sozu-proxy/sozu/commit/73c496dec0160126b370ba245f51e0b2a6765904) ] remove protocol version from response [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`d51590c`](https://github.com/sozu-proxy/sozu/commit/d51590c139aea2bb784747b091c6a866c518b1ec) ] write Response in protobuf [`Emmanuel Bosquet`] (`2023-04-28`)
- [ [`cdc8629`](https://github.com/sozu-proxy/sozu/commit/cdc8629201c9e9df51a72acdec9590e8baa8ec67) ] remove the DumpState command from the protobuf Request [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`22719bd`](https://github.com/sozu-proxy/sozu/commit/22719bd14a57bbad77d37940ea96ec45c4dc4458) ] remove JSON serialization tests [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`acb0455`](https://github.com/sozu-proxy/sozu/commit/acb0455235d08d679624cdb41526855f8fad6c61) ] isolate method SessionManager::at_capacity() [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`f9de7ba`](https://github.com/sozu-proxy/sozu/commit/f9de7bab7f1dcdb30e88b5ffa67d28a86a565891) ] create AcceptError::BufferCapacityReached, use it [`Emmanuel Bosquet`] (`2023-05-03`)
- [ [`b125338`](https://github.com/sozu-proxy/sozu/commit/b125338a160bc8a91169ed10970230c1110b6681) ] use default values wherever possible [`Emmanuel Bosquet`] (`2023-04-12`)
- [ [`edb6863`](https://github.com/sozu-proxy/sozu/commit/edb6863c9bdca8976c816b1c273cefb3ec60eab5) ] rewrite use statements [`Emmanuel Bosquet`] (`2023-04-13`)
- [ [`570f8af`](https://github.com/sozu-proxy/sozu/commit/570f8af4486590bb7b3c77a9dd80630c505e5b8a) ] implement From<ContentType> for ResponseContent [`Emmanuel Bosquet`] (`2023-05-04`)

#### ⛑️ Fixed

- [ [`4901718`](https://github.com/sozu-proxy/sozu/commit/490171851965727735e46c3efbacd564efd1bec1) ] fix nightly warning in network drain [`Emmanuel Bosquet`] (`2023-05-15`)
- [ [`564177c`](https://github.com/sozu-proxy/sozu/commit/564177c5ae09b73a470df702c88ec65ed1cf6745) ] fix examples: use statements, bugs [`Emmanuel Bosquet`] (`2023-05-09`)
- [ [`6fa15e5`](https://github.com/sozu-proxy/sozu/commit/6fa15e5617a077dd3fc0ea13a665038027efdd60) ] apply clippy fixes [`Emmanuel Bosquet`] (`2023-05-05`)
- [ [`ea0db5d`](https://github.com/sozu-proxy/sozu/commit/ea0db5dcf93ed7ff9d4069bd264b3a7b8a9b615c) ] Fix a "blue green" issue: [`Eloi DEMOLIS`] (`2023-04-13`)
- [ [`7555d38`](https://github.com/sozu-proxy/sozu/commit/7555d383acdee72e0224685d07a827faeb94217a) ] fix(proxy): Add power_of_two and least_loaded to FromStr trait [`Tim Bart`] (`2022-12-18`)
- [ [`11407b8`](https://github.com/sozu-proxy/sozu/commit/11407b8800e87caea635f365d40d4dadfb28745a) ] fix(macos): minor tweaks to for cargo build to run successfully [`Tim Bart`] (`2022-12-18`)
- [ [`384489c`](https://github.com/sozu-proxy/sozu/commit/384489c7e0ede18b2caedc20024fbe4d3e5f6e06) ] Use clippy with Rust 1.67.0 and format source code [`Florentin Dubois`] (`2023-02-06`)
- [ [`fbad528`](https://github.com/sozu-proxy/sozu/commit/fbad528ff72113072c98ffad24213917a7b9644e) ] Update return for get_executable_path (freebsd) [`3boll`] (`2023-03-01`)

### 🥹 Contributors
* @alkavan made their first contribution in https://github.com/sozu-proxy/sozu/pull/693
* @kianmeng made their first contribution in https://github.com/sozu-proxy/sozu/pull/830
* @pims made their first contribution in https://github.com/sozu-proxy/sozu/pull/868
* @Sykursen made their first contribution in https://github.com/sozu-proxy/sozu/pull/893
* @jmingov made their first contribution in https://github.com/sozu-proxy/sozu/pull/894
* @Wonshtrum
* @Keksoj
* @FlorentinDUBOIS

**Full Changelog**: https://github.com/sozu-proxy/sozu/compare/0.14.2...0.14.3

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
