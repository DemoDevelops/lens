# Third-party licenses

lens is licensed under the MIT License (see [LICENSE](LICENSE)). It also includes,
and installs, the third-party components below, each under its own license.

## Vendored query files (committed in this repository)

These tree-sitter query files are copied verbatim from their upstream grammars into
`src/discovery/vendored/` (used unmodified apart from an added attribution header):

- **`src/discovery/vendored/kotlin-tags.scm`** — from
  [fwcd/tree-sitter-kotlin](https://github.com/fwcd/tree-sitter-kotlin)
  (`queries/tags.scm`). MIT License, Copyright (c) 2019 fwcd.
- **`src/discovery/vendored/scala-tags.scm`** — from
  [tree-sitter/tree-sitter-scala](https://github.com/tree-sitter/tree-sitter-scala)
  (`queries/tags.scm`). MIT License, Copyright (c) 2018 Max Brunsfeld and GitHub.

Both are provided under the MIT License:

```
The MIT License (MIT)

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

## Installed at setup (not committed to this repository)

- **RTK (Rust Token Killer)** — [rtk-ai/rtk](https://github.com/rtk-ai/rtk),
  Apache License 2.0. lens downloads and installs RTK's prebuilt binary during
  `lens setup` as the optional shell-output compression layer. The binary is not
  bundled in this repository.

## Cargo dependencies

lens builds on many Rust crates (tree-sitter and its grammar crates, rusqlite,
rmcp, tokio, and others), each under its own permissive license (MIT or
Apache-2.0). Their license texts are distributed with the crates through Cargo; run
`cargo about generate` or `cargo deny list` for a full machine-generated report.
