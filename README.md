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

**Rust version works end to end.** Phase A (shell, tray, settings, hotkeys, the
line with springs/cards/hover, folder and clipboard capture, flight/drop) and
Phase B (hover preview, sticky notes, annotation editor, drag preview, sounds,
the full tray menu, shortcut recorder, en/es/zh) are implemented. Phase C is in
progress and measured: 1.29 MB single executable, ~105 ms to the tray, 60.5 fps
on the 6K line, 200 consecutive captures settling within 4% of the warm memory.

The original C# / WPF implementation is frozen under
[`archive/wpf-v1/`](archive/wpf-v1/) — it still builds and is the behavioural
reference.

Measured numbers and the remaining work are in [PLAN.md](PLAN.md) §3.3–§3.4 and
§8; the port's decision log is [docs/DECISIONS.md](docs/DECISIONS.md), and the
2026-11-03 visual acceptance pass (screenshots, fixed defects, frame-by-frame
results) is [docs/VISUAL-TESTS.md](docs/VISUAL-TESTS.md).

## Build

```powershell
cargo build --release          # target\release\pegline.exe
cargo test                     # pure logic only
cargo clippy --all-targets -- -D warnings
cargo run --bin probe -- --mode=flight
```

Single executable, no runtime to install. Data lives in `%APPDATA%\Pegline` and
`%LOCALAPPDATA%\Pegline` and is shared with the WPF version, so the two can be
swapped back and forth without losing the line.

Development switches: `--demo` hangs three generated cards, `--editor-demo`
opens the annotation editor on a generated picture for four seconds
(`--editor-hold` keeps it open for interactive checks), and `--panic-test`
proves the crash log and the process survive a panic. Stress the capture pipeline
with `pwsh tools\stress.ps1 -Count 200` while the release build is running.

## Documents

| | |
|:--|:--|
| [PLAN.md](PLAN.md) | milestones, performance budget, decisions, next steps |
| [docs/BEHAVIOR.md](docs/BEHAVIOR.md) | the complete behavioural spec to reproduce |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | target architecture (Win32 + DirectComposition + D3D11/D2D) |
| [docs/RUST-STACK.md](docs/RUST-STACK.md) | toolchain, crates, build, packaging, testing |
| [docs/SPIKE-01.md](docs/SPIKE-01.md) | the M0 spike: what to build, what to measure |
| [docs/PARITY.md](docs/PARITY.md) | acceptance checklist for the port |
| [docs/DECISIONS.md](docs/DECISIONS.md) | implementation choices and rejected optimisations |
| [docs/MEMORY-ANALYSIS.md](docs/MEMORY-ANALYSIS.md) | why: measured memory and frame rates of the WPF version |
| [AGENTS.md](AGENTS.md) | working notes for agents |

## Licence

MIT. See [LICENSE](LICENSE).
