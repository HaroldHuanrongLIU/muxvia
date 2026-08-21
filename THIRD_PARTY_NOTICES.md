# Third-party notices

Muxvia is distributed under the MIT License. Its source-derived compatibility work and interaction design also preserve these notices. Inclusion does not imply affiliation with or endorsement by the named projects.

## CC-Switch

Source: `farion1231/cc-switch` at `43eaf07355af145aebfee301801779e824d4c221` (`v3.19.2`)

MIT License

Copyright (c) 2025 Jason Young

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

## OpenCode

Source: `anomalyco/opencode` at `0abbcddac233e313bcb67608a527929910df861c`

Muxvia independently implements the interaction grammar described in its research record; it does not copy OpenCode source text. The upstream notice is retained for the visual and interaction baseline.

MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

## Bundled libraries and runtimes

The release executables contain third-party libraries under their upstream licenses. The principal runtime dependencies are:

| Component | License |
| --- | --- |
| Bun runtime | MIT and bundled component licenses |
| OpenTUI (`@opentui/core`, `@opentui/keymap`, `@opentui/solid`) | MIT |
| SolidJS | MIT |
| Zod | MIT |
| web-tree-sitter / Tree-sitter | MIT |
| Rust crates Axum, Clap, Tokio, URL, UUID, Serde, tempfile, thiserror, flate2, futures-util, secrecy, subtle and related dependencies | MIT or MIT/Apache-2.0, as identified by each crate |
| reqwest and rustls | MIT/Apache-2.0 |
| ring | ISC, OpenSSL, and BSD-style component terms |
| rusqlite / bundled SQLite | MIT wrapper; SQLite public domain |

The authoritative dependency versions are locked in `bun.lock` and `Cargo.lock`. Their source and license metadata are available from the corresponding package registries and upstream repositories.
