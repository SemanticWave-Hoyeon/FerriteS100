# Lua runtime selection

`ferrite-lua-runtime` owns the interpreter and its build-time backend. `ferrite-lua` retains S-100 host callbacks, rule loading and its existing sandbox policy. Product adapters use the same host API for both backends.

Exactly one backend must be selected. The application defaults to Lua 5.4. From the repository root:

```sh
cargo build --locked
cargo build --locked --no-default-features --features lua55
cargo test --locked --workspace
cargo test --locked --workspace --no-default-features --features lua55
```

For the standalone host package:

```sh
cargo test --locked -p ferrite-lua
cargo test --locked -p ferrite-lua --no-default-features --features lua55
```

The standalone `ferrite-s101` and `ferrite-s102` packages expose the same `lua54`/`lua55` features and default to Lua 5.4. The root application forwards its selected backend to both adapters.

Selecting Lua 5.5 requires rebuilding the application. This package is not a dynamic plugin ABI and does not permit exchanging Lua values between different runtimes. `--all-features` is intentionally unsupported because it would enable two incompatible interpreter backends.

The runtime pins mlua 0.11.6 with vendored Lua sources for reproducibility. Runtime and host tests must be run for each backend; successful VM creation alone does not establish catalogue compatibility. A Lua 5.5 option does not imply that an IHO catalogue requires Lua 5.5.
