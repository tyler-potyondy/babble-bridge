# nrf-sim-bridge

BabbleSim + Zephyr nRF RPC simulation bridge. Provides:

- **Test harness** — spawn a full BabbleSim simulation from Rust integration tests
- **xtask CLI** — `docker-build`, `docker-attach`, `zephyr-setup`, `run-bsim`

## Using xtask commands from a downstream crate

### 1. Define the dependency once at workspace level

Root `Cargo.toml`:

```toml
[workspace]
members = ["your-app", "xtask"]

[workspace.dependencies]
nrf-sim-bridge = { git = "https://github.com/your-org/nrf-sim-bridge.git" }
```

### 2. Create an xtask crate

```bash
cargo init xtask
```

`xtask/Cargo.toml`:

```toml
[package]
name = "xtask"
version = "0.1.0"
edition = "2021"

[dependencies]
nrf-sim-bridge.workspace = true
```

`xtask/src/main.rs`:

```rust
fn main() {
    nrf_sim_bridge::xtask::cli_main();
}
```

### 3. Add the cargo alias

`.cargo/config.toml`:

```toml
[alias]
xtask = "run -p xtask --"
```

### 4. Use it

```bash
cargo xtask docker-build
cargo xtask docker-attach
cargo xtask zephyr-setup --prebuilt
cargo xtask run-bsim
```

All commands run in the context of your workspace root — Docker
bind-mounts your project, binaries land in your `external/` directory.

## Test harness

Add to your crate's `Cargo.toml` (inherits the git source from the
workspace if you set up `[workspace.dependencies]` above):

```toml
[dev-dependencies]
nrf-sim-bridge.workspace = true
```

In an integration test:

```rust
use std::collections::HashSet;
use std::path::Path;

let tests_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/sockets"));
let (mut processes, socket_path) =
    nrf_sim_bridge::spawn_zephyr_rpc_server_with_socat(tests_dir, "my_test");

// Connect to socket_path with a UnixStream, exchange bytes, then:
processes.search_stdout_for_strings(HashSet::from([
    "<inf> nrf_ps_server: Initializing RPC server",
]));
```

Enable `sim-log` to see labelled process output during tests:

```bash
cargo test --features nrf-sim-bridge/sim-log
```
