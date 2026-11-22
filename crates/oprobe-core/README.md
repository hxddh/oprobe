# oprobe-core

**Internal.** This crate is the engine of the [oprobe](https://github.com/hxddh/oprobe) binary:
the operator spec, workloads, the profile and its findings, `prove`, the run store and the lab.
It is published only so that `cargo install oprobe` works. It has **no semver promise**: any
release may change any item, and `oprobe` pins it with an exact version.

- To profile your own OpenDAL operator, use [`oprobe-trace`](https://crates.io/crates/oprobe-trace).
- To use oprobe, install the binary: `cargo install oprobe --locked`.

License: Apache-2.0.
