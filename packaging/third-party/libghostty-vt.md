## libghostty-vt and its bundled dependencies

The terminal panel is backed by `lamina-term`, which links a static
libghostty-vt 1.0.1. That library is not a Rust crate: `crates/lamina-term/build.rs`
downloads the pinned libghostty-vt source and dependency tarballs (verifying
their SHA-256 sums) and compiles the static library with Zig 0.17.1, so the
code below ends up inside the Lamina binaries but never appears in the crate
graph that `cargo-about` walks.

libghostty-vt is MIT licensed. Its build pulls in the following projects;
"linked" means the code is compiled into the static library, "build" means it
only runs while building it. The set matches the upstream
`deps/libghostty-vt-third-party.json` for the pinned release.

| Component | Version | Licence | Use |
| --- | --- | --- | --- |
| libghostty-vt | 1.0.1 | MIT | linked |
| aro | 0.0.0 | MIT AND Unicode-3.0 | build |
| highway | 1.2.0 | Apache-2.0 OR BSD-3-Clause | linked |
| simdutf | 9.0.0 | Apache-2.0 OR MIT | linked |
| translate_c | 2.0.0 | MIT | linked |
| uucode | 0.2.0 | MIT AND Unicode-3.0 | linked |
| wuffs | 0.4.0-alpha.10 | Apache-2.0 OR MIT | linked |

The MIT, Apache-2.0, BSD-3-Clause and Unicode-3.0 texts in the licence section
above cover these components. libghostty-vt carries its own MIT notice, which
is reproduced here in full:

```
MIT License

Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
