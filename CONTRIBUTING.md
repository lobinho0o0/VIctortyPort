# Contributing to VIctory

VIctory is a Rust workspace. It builds on Linux x86-64 and Windows x64. The
tests and the data and shader tools run on both. Running a title needs Windows
10 or 11 x64, an AVX2 CPU and a Direct3D 12 GPU. The tests need no game data.
See [README.md](README.md) for what the project does today.

## Set up your workspace

1. Install Rust with rustup. `rust-toolchain.toml` pins the toolchain. rustup
   installs it on first use.
2. On Windows, install the Visual Studio build tools with C++ support. On
   Linux, install a C toolchain such as GCC or Clang if `cc` is not present;
   Rust uses `cc` as its linker.
3. Run `cargo xtask setup` in the repository root. The command installs the git
   hooks and creates `victory.toml` when it is missing. It copies
   `victory.example.toml` on Windows and writes paths under your home folder on
   Linux. Run it again at any time; it keeps your existing `victory.toml`.
4. Edit `victory.toml`. Set every path outside the repository. The tools refuse
   paths inside the repository.
5. Run `cargo test --workspace` to check your setup.

Plain `victory translate` works on Linux. Its `--validate` and `--pipelines`
checks need Windows, as does `victory run`.

## Make a change

1. Create a branch for your change.
2. Make the change. Add or update tests with it.
3. Run `cargo test --workspace` and `cargo xtask policy`.
4. Commit. The pre-commit hook runs `cargo xtask policy --staged`. It stops a
   commit that breaks the asset policy.
5. Open a pull request. Describe the change and list the checks you ran.

## Keep game data out of the repository

The repository holds only small text files that we write. Do not commit game
files, dumps, shaders, textures, logs with game data, or configuration with
your paths. [docs/asset-policy.md](docs/asset-policy.md) gives the full rule.
`.gitignore` and the policy check are two defences; they do not replace your
own check.

## Get help

Open an issue. Include the command you ran, its output, and your OS and GPU.
