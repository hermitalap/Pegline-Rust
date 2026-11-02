# Pegline

Screenshots hang on a clothesline along the top of the screen. Push the pointer
against the top edge and the line slides down; move away and it tucks back up.

Pegline is a Windows port of [Tendedero](https://github.com/alejandrobujan/tendedero).
The name and the icon are not covered by the upstream licence, so this port has its own.

## Goals

Small, light, fast — targets, not hard limits:

- **Small**: one executable, no installer, a few MB to ~15 MB.
- **Light**: tens of MB of private memory at rest.
- **Fast**: a stable 60 fps, with 120 fps on 120 Hz displays.

The first release is an MVP of the core loop; optimisation rounds follow, each
with measured before/after numbers.

## Status

**Being rewritten in Rust.** The original C# / WPF implementation is complete and
frozen under [`archive/wpf-v1/`](archive/wpf-v1/) — it still builds, runs, and is the
reference for behaviour. No Rust code exists yet: the repository is at the end of
the design phase.

The first thing to build is the compositing spike described in
[`docs/SPIKE-01.md`](docs/SPIKE-01.md). It decides whether the architecture works
before the port starts.

## Documents

| | |
|:--|:--|
| [PLAN.md](PLAN.md) | milestones, performance budget, decisions, next steps |
| [docs/BEHAVIOR.md](docs/BEHAVIOR.md) | the complete behavioural spec to reproduce |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | target architecture (Win32 + DirectComposition + D3D11/D2D) |
| [docs/RUST-STACK.md](docs/RUST-STACK.md) | toolchain, crates, build, packaging, testing |
| [docs/SPIKE-01.md](docs/SPIKE-01.md) | the M0 spike: what to build, what to measure |
| [docs/PARITY.md](docs/PARITY.md) | acceptance checklist for the port |
| [docs/MEMORY-ANALYSIS.md](docs/MEMORY-ANALYSIS.md) | why: measured memory and frame rates of the WPF version |
| [AGENTS.md](AGENTS.md) | working notes for agents |

## Build (planned)

```powershell
cargo build --release
target\release\pegline.exe
```

Single executable, no runtime to install. Data lives in `%APPDATA%\Pegline` and
`%LOCALAPPDATA%\Pegline` and is shared with the WPF version, so the two can be
swapped back and forth without losing the line.

## Licence

MIT. See [LICENSE](LICENSE).
