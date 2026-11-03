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

The current release, **2.1.0**, ships the full feature set together with the
optimisation work measured so far.

## Status

**Pegline 2.1.0 is the current release (2026-11-03); 2.0.0 was the first complete
Rust release.** Phase A (shell, tray, settings, hotkeys, the line with
springs/cards/hover, folder and clipboard capture, flight/drop) and Phase B
(hover preview, sticky notes, annotation editor, drag preview, sounds, the full
tray menu, shortcut recorder, en/es/zh) are complete. Measured on the
development machine: a 1.34 MB single executable, 81–86 ms to the tray with a
warm cache, 60.3 fps on the 6K line, 61.5 fps for a 1600 px flight, and 200
consecutive captures settling within 1.5% of the warm memory.

The remaining checks are hardware-specific (120 Hz, a second monitor,
sleep/wake) or subjective (the three audio cues); they are recorded as
limitations rather than invented numbers.

The original C# / WPF implementation is frozen under
[`archive/wpf-v1/`](archive/wpf-v1/) — it still builds and is the behavioural
reference.

Measured numbers and the remaining work are in [PLAN.md](PLAN.md) §3.3–§3.5 and
§8; the port's decision log is [docs/DECISIONS.md](docs/DECISIONS.md), the
2026-11-03 visual acceptance pass is [docs/VISUAL-TESTS.md](docs/VISUAL-TESTS.md),
and the release notes are [docs/RELEASE-2.0.0.md](docs/RELEASE-2.0.0.md).

## Release

The portable release is `dist/Pegline-2.1.0-win-x64.zip`: copy `pegline.exe`
anywhere and run it. There is no installer and no runtime. Build the same
package with:

```powershell
pwsh tools\package-release.ps1
```

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
proves the crash log and the process survive a panic. `PEGLINE_LANG=en|es|zh`
forces a UI language for the three-language layout checks. Stress the capture
pipeline with `pwsh tools\stress.ps1 -Count 200` while the release build is
running.

## Documents

| | |
|:--|:--|
| [PLAN.md](PLAN.md) | milestones, performance budget, decisions, next steps |
| [CHANGELOG.md](CHANGELOG.md) | what changed in 2.1.0 |
| [docs/RELEASE-2.0.0.md](docs/RELEASE-2.0.0.md) | 2.0.0 release notes, checksum and known limitations (historical) |
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
