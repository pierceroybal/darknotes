# Third-party notices

darknotes itself is dual-licensed MIT OR Apache-2.0 (see [`LICENSE-MIT`](LICENSE-MIT)
and [`LICENSE-APACHE`](LICENSE-APACHE)). It also **embeds** third-party assets
directly into the compiled binary, so anyone redistributing that binary —
not just the source — carries the obligations below.

## Embedded in the binary

These are compiled in via `include_bytes!` (`src/main.rs`), which means every
`darknotes` executable is a redistribution of them.

| Asset | Use | License | Full text |
| --- | --- | --- | --- |
| [Courier Prime](https://github.com/quoteunquoteapps/CourierPrime) (Regular, Bold, Italic, BoldItalic) | Default editor font | SIL Open Font License 1.1 | [`assets/fonts/OFL-CourierPrime.txt`](assets/fonts/OFL-CourierPrime.txt) |
| [Inter](https://github.com/rsms/inter) (Regular, Italic) | Default UI chrome font | SIL Open Font License 1.1 | [`assets/fonts/OFL-Inter.txt`](assets/fonts/OFL-Inter.txt) |
| [Lucide](https://lucide.dev) (7 SVG icons) | Sidebar and tab icons | ISC | [`assets/icons/LICENSE-lucide.txt`](assets/icons/LICENSE-lucide.txt) |

Both the OFL and the ISC license require that the copyright notice and license
text travel with any redistribution. Practically, that means:

- **Source redistribution / forks** — keep the `assets/` directory intact. The
  license files sit beside the assets they cover, so this is automatic.
- **Binary redistribution** — ship the three license files listed above
  alongside the executable, or point to this file. The release workflow
  (`.github/workflows/release.yml`) bundles them into every archive, so the
  published releases already satisfy this.

Neither license requires that darknotes adopt their terms: the OFL's copyleft
covers the font files themselves, not software that renders with them, and the
ISC is permissive. Dual MIT/Apache-2.0 for darknotes is compatible with both.

The OFL does forbid selling the fonts *on their own*, and reserves the right to
rename a modified font. Shipping them unmodified inside an application, as
darknotes does, is the ordinary permitted use.

## Rust dependencies

Every crate in `Cargo.lock` is fetched at build time rather than vendored into
this repository, so their source is not redistributed here — but they are
statically linked into the binary you ship.

The direct dependencies, with the license each crate declares:

| Crate | Purpose | License |
| --- | --- | --- |
| [`gpui`](https://crates.io/crates/gpui) | GPU-accelerated UI framework (from Zed) | Apache-2.0 |
| [`jiff`](https://crates.io/crates/jiff) | Dates and times for daily notes and task scheduling | Unlicense OR MIT |
| [`notify`](https://crates.io/crates/notify) | Filesystem watching for external edits | CC0-1.0 |
| [`nucleo-matcher`](https://crates.io/crates/nucleo-matcher) | Fuzzy matching in the picker | MPL-2.0 |
| [`open`](https://crates.io/crates/open) | Opening URLs in the system browser | MIT |
| [`ropey`](https://crates.io/crates/ropey) | The editor's text rope | MIT OR Apache-2.0 |
| [`serde`](https://crates.io/crates/serde) | Config and session deserialization | MIT OR Apache-2.0 |
| [`toml`](https://crates.io/crates/toml) | Config and session file format | MIT OR Apache-2.0 |

One of these is weak-copyleft and worth naming explicitly:

- **`nucleo-matcher` (MPL-2.0)** — the MPL's copyleft is *file-scoped*. It
  reaches modifications to nucleo's own source files, not code that merely calls
  its API, and it explicitly permits distributing a "Larger Work" under other
  terms. darknotes links it unmodified, so nothing propagates. Should that ever
  become unwelcome, the picker's matcher is a narrow, replaceable seam.

`notify` is CC0-1.0, a public-domain dedication that imposes nothing at all.
`gpui` is Apache-2.0, whose only real obligation is preserving its notices —
compatible with darknotes' own dual MIT/Apache-2.0 in either direction.

### The full tree

A spot audit of the 708 packages in `Cargo.lock` found only permissive terms
(MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, Unlicense, CC0) plus the three
MPL-2.0 crates noted above, and no GPL/LGPL/AGPL anywhere. That audit read the
~500 packages present in the local registry cache; it is a good signal, not a
substitute for a real scan. Before your first binary release, run one:

```sh
cargo install cargo-deny && cargo deny check licenses
```

To regenerate a complete, transitive license inventory as a browsable page:

```sh
cargo install cargo-about && cargo about generate --output-file licenses.html
# or, for a quick audit:
cargo install cargo-deny && cargo deny check licenses
```
