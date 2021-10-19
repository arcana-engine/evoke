# lloth

[![crates](https://img.shields.io/crates/v/lloth.svg?style=for-the-badge&label=lloth)](https://crates.io/crates/lloth)
[![docs](https://img.shields.io/badge/docs.rs-lloth-66c2a5?style=for-the-badge&labelColor=555555&logoColor=white)](https://docs.rs/lloth)
[![actions](https://img.shields.io/github/workflow/status/zakarumych/lloth/badge/master?style=for-the-badge)](https://github.com/zakarumych/lloth/actions?query=workflow%3ARust)
[![MIT/Apache](https://img.shields.io/badge/license-MIT%2FApache-blue.svg?style=for-the-badge)](COPYING)
![loc](https://img.shields.io/tokei/lines/github/zakarumych/lloth?style=for-the-badge)

Lloth provides basic building blocks to add networking capabilities to game engine.

Lloth supports:
* Client-Server model with authoritative server

For client-server model `lloth` automatically performs state replication with delta compression from server to client\
and commands replication from client to server.

Lloth makes no assumption of game loop and components used in game.\
User needs to register `lloth::server::Descriptor` in server and `lloth::client::Descriptor` in client for components that need to be replicated. There's blanket implementation for components that are comparable for equality and serializable.

## License

Licensed under either of

* Apache License, Version 2.0, ([license/APACHE](license/APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([license/MIT](license/MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
