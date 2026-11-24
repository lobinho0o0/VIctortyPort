# Running VIctory with the Proton launcher

Install `umu-run` on x86-64 Linux. The game requires an AVX2 CPU and a Vulkan
driver that exposes Shader Model 6.6 through vkd3d-proton.
[UMU installation](https://github.com/Open-Wine-Components/umu-launcher#installation)

Use a Windows x64 build of VIctory. The launcher supports MSVC builds,
including those produced by `cargo xwin`, and LLVM-MinGW builds. Both Rust
targets use native Windows thread-local storage (TLS). A loader test with
the GCC-based `x86_64-pc-windows-gnu` target aborted when a title DLL called
into Rust during thread startup.

```sh
./scripts/victory-proton.sh "/path/to/game/title.exe" --execute
```

Omit `--execute` to check mapping and import binding first. The launcher
forwards other `victory run` options and arguments after `--`. To select a
specific build, set `VICTORY_EXE` to its Linux path. By default the launcher
looks beside itself for `victory.exe`, then under `${CARGO_TARGET_DIR:-target}`
for the gnullvm and MSVC release builds, in that order.

On first launch UMU fetches Proton and the Steam Linux Runtime. The default
prefix is `${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton`, and the
default `GAMEID` is `umu-victory`. Set `WINEPREFIX`, `GAMEID` or `PROTONPATH`
to override them. Use Windows paths in `victory.toml`.

## Building with LLVM-MinGW

Install Rust with rustup and put the x86-64 Linux UCRT distribution of
LLVM-MinGW first on `PATH` (its `bin` directory).
[LLVM-MinGW releases](https://github.com/mstorsjo/llvm-mingw/releases)

```sh
rustup target add x86_64-pc-windows-gnullvm
cargo build --locked --release -p victory --target x86_64-pc-windows-gnullvm
```

## Testing without game files

`./tests/proton-smoke.sh` compiles its synthetic title with the LLVM-MinGW
toolchain and gnullvm Rust target above, even when `VICTORY_EXE` points to
an MSVC build. The test checks the launcher's exit status and the return
value from the title. It uses a separate `victory/proton-smoke` prefix under
the XDG data directory. Set `VICTORY_SMOKE_PREFIX` to choose another test prefix.

To reproduce the loader integration tests after the smoke test, build the
DLL fixture on Linux and set `VICTORY_HLE_PROBE` to the Windows path of the
built DLL. This lets the test load it without starting Cargo inside Proton.
Run these commands from the checkout in Bash.

```sh
target=x86_64-pc-windows-gnullvm
fixture_dir=$(realpath -m "${CARGO_TARGET_DIR:-target}/proton-fixture")
cargo build --locked --manifest-path tests/fixtures/hle-probe/Cargo.toml \
    --target "$target" --target-dir "$fixture_dir"
export VICTORY_HLE_PROBE="Z:$fixture_dir/$target/debug/hle_probe.dll"
export WINEPREFIX=${VICTORY_SMOKE_PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton-smoke}
PROTON_VERB=runinprefix GAMEID=umu-victory CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER=umu-run \
    cargo test --locked --target "$target" -p victory-loader --test load_probe -- --test-threads=1
```
