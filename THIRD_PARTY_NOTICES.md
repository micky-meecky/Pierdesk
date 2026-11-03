# Third-party notices

Pierdesk is released under the GNU General Public License v3.0 (`GPL-3.0-only`, see [LICENSE](./LICENSE)).
It includes, embeds or links the third-party software listed here. Each component remains under its own license.
This file lists them and says where the license texts are; it does not replace those licenses.

## 1. Bundled binary: Win32-OpenSSH

`pierdesk.exe` embeds the unmodified `OpenSSH-Win64.zip` from the Microsoft-maintained
[PowerShell/Win32-OpenSSH](https://github.com/PowerShell/Win32-OpenSSH) release `10.0.0.0p2-Preview`.
It is used by **Remote work** and is installed only when you set up local SSH access. Before installing it, Pierdesk
checks the archive's fixed SHA-256 digest and the Microsoft Authenticode signatures.

- SHA-256 of the archive: `23F50F3458C4C5D0B12217C6A5DDFDE0137210A30FA870E98B29827F7B43ABA5`
- The archive keeps its original `LICENSE.txt` and `NOTICE.txt`. They cover OpenSSH (BSD-style and ISC-style licenses)
  and the other components shipped in it, including OpenSSL's `libcrypto.dll`.
- The same archive is in this repository at [`assets/openssh/`](./assets/openssh/); see its [README](./assets/openssh/README.md).
- Source code: the upstream release and tag linked above.

## 2. Vendored source: visual region detector

[`vendor/snow-visual-region-detector`](./vendor/snow-visual-region-detector) is source code from the
[Snow Apps](https://github.com/mg-chao/snow-apps) project (`snow-crates/crates/visual-region-detector`, pinned revision
`fc8f669b583cb889b8b20057013a02e8d41aa07f`), used by the screenshot editor.

- License: GPL-3.0-or-later. The upstream license text is kept in [its LICENSE file](./vendor/snow-visual-region-detector/LICENSE).
- Copyright belongs to the Snow Apps contributors.
- Changes made for Pierdesk are described in [its UPSTREAM.md](./vendor/snow-visual-region-detector/UPSTREAM.md).

Because it is GPL-3.0-or-later, it can be combined with Pierdesk under GPL-3.0.

## 3. Rust crates in the default build

These 82 crates from [crates.io](https://crates.io) are compiled into the default (community) build. The list comes from
`cargo metadata` for the `x86_64-pc-windows-msvc` target and matches `Cargo.lock`. License identifiers are SPDX expressions
as declared by each crate; where a crate offers a choice (`OR`), Pierdesk may use it under either.

| Crate | Version | License |
| --- | --- | --- |
| [asn1-rs](https://crates.io/crates/asn1-rs/0.7.2) | 0.7.2 | MIT OR Apache-2.0 |
| [asn1-rs-derive](https://crates.io/crates/asn1-rs-derive/0.6.0) | 0.6.0 | MIT OR Apache-2.0 |
| [asn1-rs-impl](https://crates.io/crates/asn1-rs-impl/0.2.0) | 0.2.0 | MIT/Apache-2.0 |
| [autocfg](https://crates.io/crates/autocfg/1.5.1) | 1.5.1 | Apache-2.0 OR MIT |
| [base64](https://crates.io/crates/base64/0.22.1) | 0.22.1 | MIT OR Apache-2.0 |
| [bit-vec](https://crates.io/crates/bit-vec/0.9.1) | 0.9.1 | Apache-2.0 OR MIT |
| [block-buffer](https://crates.io/crates/block-buffer/0.10.4) | 0.10.4 | MIT OR Apache-2.0 |
| [bytemuck](https://crates.io/crates/bytemuck/1.25.2) | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| [byteorder-lite](https://crates.io/crates/byteorder-lite/0.1.0) | 0.1.0 | Unlicense OR MIT |
| [cc](https://crates.io/crates/cc/1.4.4) | 1.4.4 | MIT OR Apache-2.0 |
| [cfg-if](https://crates.io/crates/cfg-if/1.0.4) | 1.0.4 | MIT OR Apache-2.0 |
| [cpufeatures](https://crates.io/crates/cpufeatures/0.2.17) | 0.2.17 | MIT OR Apache-2.0 |
| [crypto-common](https://crates.io/crates/crypto-common/0.1.7) | 0.1.7 | MIT OR Apache-2.0 |
| [data-encoding](https://crates.io/crates/data-encoding/2.11.1) | 2.11.1 | MIT |
| [der-parser](https://crates.io/crates/der-parser/10.0.0) | 10.0.0 | MIT OR Apache-2.0 |
| [deranged](https://crates.io/crates/deranged/0.5.8) | 0.5.8 | MIT OR Apache-2.0 |
| [digest](https://crates.io/crates/digest/0.10.7) | 0.10.7 | MIT OR Apache-2.0 |
| [displaydoc](https://crates.io/crates/displaydoc/0.2.7) | 0.2.7 | MIT OR Apache-2.0 |
| [find-msvc-tools](https://crates.io/crates/find-msvc-tools/0.1.11) | 0.1.11 | MIT OR Apache-2.0 |
| [generic-array](https://crates.io/crates/generic-array/0.14.7) | 0.14.7 | MIT |
| [getrandom](https://crates.io/crates/getrandom/0.2.17) | 0.2.17 | MIT OR Apache-2.0 |
| [getrandom](https://crates.io/crates/getrandom/0.3.4) | 0.3.4 | MIT OR Apache-2.0 |
| [image](https://crates.io/crates/image/0.25.10) | 0.25.10 | MIT OR Apache-2.0 |
| [itoa](https://crates.io/crates/itoa/1.0.18) | 1.0.18 | MIT OR Apache-2.0 |
| [lazy_static](https://crates.io/crates/lazy_static/1.5.0) | 1.5.0 | MIT OR Apache-2.0 |
| [memchr](https://crates.io/crates/memchr/2.8.3) | 2.8.3 | Unlicense OR MIT |
| [minimal-lexical](https://crates.io/crates/minimal-lexical/0.2.1) | 0.2.1 | MIT/Apache-2.0 |
| [moxcms](https://crates.io/crates/moxcms/0.8.1) | 0.8.1 | BSD-3-Clause OR Apache-2.0 |
| [nom](https://crates.io/crates/nom/7.1.3) | 7.1.3 | MIT |
| [num-bigint](https://crates.io/crates/num-bigint/0.4.8) | 0.4.8 | MIT OR Apache-2.0 |
| [num-conv](https://crates.io/crates/num-conv/0.2.2) | 0.2.2 | MIT OR Apache-2.0 |
| [num-integer](https://crates.io/crates/num-integer/0.1.47) | 0.1.47 | MIT OR Apache-2.0 |
| [num-traits](https://crates.io/crates/num-traits/0.2.19) | 0.2.19 | MIT OR Apache-2.0 |
| [oid-registry](https://crates.io/crates/oid-registry/0.8.1) | 0.8.1 | MIT OR Apache-2.0 |
| [once_cell](https://crates.io/crates/once_cell/1.21.4) | 1.21.4 | MIT OR Apache-2.0 |
| [powerfmt](https://crates.io/crates/powerfmt/0.2.0) | 0.2.0 | MIT OR Apache-2.0 |
| [proc-macro2](https://crates.io/crates/proc-macro2/1.0.107) | 1.0.107 | MIT OR Apache-2.0 |
| [pxfm](https://crates.io/crates/pxfm/0.1.30) | 0.1.30 | BSD-3-Clause OR Apache-2.0 |
| [quote](https://crates.io/crates/quote/1.0.47) | 1.0.47 | MIT OR Apache-2.0 |
| [rcgen](https://crates.io/crates/rcgen/0.14.10) | 0.14.10 | MIT OR Apache-2.0 |
| [ring](https://crates.io/crates/ring/0.17.14) | 0.17.14 | Apache-2.0 AND ISC |
| [rusticata-macros](https://crates.io/crates/rusticata-macros/4.1.0) | 4.1.0 | MIT/Apache-2.0 |
| [rustls](https://crates.io/crates/rustls/0.23.43) | 0.23.43 | Apache-2.0 OR ISC OR MIT |
| [rustls-pki-types](https://crates.io/crates/rustls-pki-types/1.15.1) | 1.15.1 | MIT OR Apache-2.0 |
| [rustls-webpki](https://crates.io/crates/rustls-webpki/0.103.15) | 0.103.15 | ISC |
| [serde](https://crates.io/crates/serde/1.0.229) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_core](https://crates.io/crates/serde_core/1.0.229) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_derive](https://crates.io/crates/serde_derive/1.0.229) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_json](https://crates.io/crates/serde_json/1.0.151) | 1.0.151 | MIT OR Apache-2.0 |
| [sha2](https://crates.io/crates/sha2/0.10.9) | 0.10.9 | MIT OR Apache-2.0 |
| [shlex](https://crates.io/crates/shlex/2.0.1) | 2.0.1 | MIT OR Apache-2.0 |
| [subtle](https://crates.io/crates/subtle/2.6.1) | 2.6.1 | BSD-3-Clause |
| [syn](https://crates.io/crates/syn/3.0.4) | 3.0.4 | MIT OR Apache-2.0 |
| [syn](https://crates.io/crates/syn/2.0.119) | 2.0.119 | MIT OR Apache-2.0 |
| [synstructure](https://crates.io/crates/synstructure/0.13.2) | 0.13.2 | MIT |
| [thiserror](https://crates.io/crates/thiserror/2.0.20) | 2.0.20 | MIT OR Apache-2.0 |
| [thiserror-impl](https://crates.io/crates/thiserror-impl/2.0.20) | 2.0.20 | MIT OR Apache-2.0 |
| [time](https://crates.io/crates/time/0.3.55) | 0.3.55 | MIT OR Apache-2.0 |
| [time-core](https://crates.io/crates/time-core/0.1.9) | 0.1.9 | MIT OR Apache-2.0 |
| [time-macros](https://crates.io/crates/time-macros/0.2.32) | 0.2.32 | MIT OR Apache-2.0 |
| [typenum](https://crates.io/crates/typenum/1.20.1) | 1.20.1 | MIT OR Apache-2.0 |
| [unicode-ident](https://crates.io/crates/unicode-ident/1.0.24) | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| [untrusted](https://crates.io/crates/untrusted/0.9.0) | 0.9.0 | ISC |
| [version_check](https://crates.io/crates/version_check/0.9.5) | 0.9.5 | MIT/Apache-2.0 |
| [webview2-com](https://crates.io/crates/webview2-com/0.39.1) | 0.39.1 | MIT |
| [webview2-com-macros](https://crates.io/crates/webview2-com-macros/0.8.1) | 0.8.1 | MIT |
| [webview2-com-sys](https://crates.io/crates/webview2-com-sys/0.39.1) | 0.39.1 | MIT |
| [windows](https://crates.io/crates/windows/0.62.2) | 0.62.2 | MIT OR Apache-2.0 |
| [windows-collections](https://crates.io/crates/windows-collections/0.3.2) | 0.3.2 | MIT OR Apache-2.0 |
| [windows-core](https://crates.io/crates/windows-core/0.62.2) | 0.62.2 | MIT OR Apache-2.0 |
| [windows-future](https://crates.io/crates/windows-future/0.3.2) | 0.3.2 | MIT OR Apache-2.0 |
| [windows-implement](https://crates.io/crates/windows-implement/0.60.2) | 0.60.2 | MIT OR Apache-2.0 |
| [windows-interface](https://crates.io/crates/windows-interface/0.59.3) | 0.59.3 | MIT OR Apache-2.0 |
| [windows-link](https://crates.io/crates/windows-link/0.2.1) | 0.2.1 | MIT OR Apache-2.0 |
| [windows-numerics](https://crates.io/crates/windows-numerics/0.3.1) | 0.3.1 | MIT OR Apache-2.0 |
| [windows-result](https://crates.io/crates/windows-result/0.4.1) | 0.4.1 | MIT OR Apache-2.0 |
| [windows-strings](https://crates.io/crates/windows-strings/0.5.1) | 0.5.1 | MIT OR Apache-2.0 |
| [windows-threading](https://crates.io/crates/windows-threading/0.2.1) | 0.2.1 | MIT OR Apache-2.0 |
| [x509-parser](https://crates.io/crates/x509-parser/0.18.1) | 0.18.1 | MIT OR Apache-2.0 |
| [yasna](https://crates.io/crates/yasna/0.6.0) | 0.6.0 | MIT OR Apache-2.0 |
| [zeroize](https://crates.io/crates/zeroize/1.9.0) | 1.9.0 | Apache-2.0 OR MIT |
| [zmij](https://crates.io/crates/zmij/1.0.23) | 1.0.23 | MIT |

## 4. Additional crates when built with `--features licensing`

The optional `licensing` feature is **off by default** and is not part of the community build. If you enable it, these
31 more crates are included (plus the workspace crates `clipferry-license-protocol` and `clipferry-license-server`, which
are part of this repository and are covered by Pierdesk's own license).

| Crate | Version | License |
| --- | --- | --- |
| [form_urlencoded](https://crates.io/crates/form_urlencoded/1.2.2) | 1.2.2 | MIT OR Apache-2.0 |
| [icu_collections](https://crates.io/crates/icu_collections/2.3.0) | 2.3.0 | Unicode-3.0 |
| [icu_locale_core](https://crates.io/crates/icu_locale_core/2.3.0) | 2.3.0 | Unicode-3.0 |
| [icu_normalizer](https://crates.io/crates/icu_normalizer/2.3.0) | 2.3.0 | Unicode-3.0 |
| [icu_normalizer_data](https://crates.io/crates/icu_normalizer_data/2.3.0) | 2.3.0 | Unicode-3.0 |
| [icu_properties](https://crates.io/crates/icu_properties/2.3.0) | 2.3.0 | Unicode-3.0 |
| [icu_properties_data](https://crates.io/crates/icu_properties_data/2.3.0) | 2.3.0 | Unicode-3.0 |
| [icu_provider](https://crates.io/crates/icu_provider/2.3.1) | 2.3.1 | Unicode-3.0 |
| [idna](https://crates.io/crates/idna/1.1.0) | 1.1.0 | MIT OR Apache-2.0 |
| [idna_adapter](https://crates.io/crates/idna_adapter/1.2.2) | 1.2.2 | Apache-2.0 OR MIT |
| [litemap](https://crates.io/crates/litemap/0.8.3) | 0.8.3 | Unicode-3.0 |
| [log](https://crates.io/crates/log/0.4.34) | 0.4.34 | MIT OR Apache-2.0 |
| [percent-encoding](https://crates.io/crates/percent-encoding/2.3.2) | 2.3.2 | MIT OR Apache-2.0 |
| [potential_utf](https://crates.io/crates/potential_utf/0.1.6) | 0.1.6 | Unicode-3.0 |
| [smallvec](https://crates.io/crates/smallvec/1.16.1) | 1.16.1 | MIT OR Apache-2.0 |
| [stable_deref_trait](https://crates.io/crates/stable_deref_trait/1.2.1) | 1.2.1 | MIT OR Apache-2.0 |
| [synstructure](https://crates.io/crates/synstructure/0.14.0) | 0.14.0 | MIT |
| [tinystr](https://crates.io/crates/tinystr/0.8.4) | 0.8.4 | Unicode-3.0 |
| [ureq](https://crates.io/crates/ureq/2.12.1) | 2.12.1 | MIT OR Apache-2.0 |
| [url](https://crates.io/crates/url/2.5.8) | 2.5.8 | MIT OR Apache-2.0 |
| [utf8_iter](https://crates.io/crates/utf8_iter/1.0.4) | 1.0.4 | Apache-2.0 OR MIT |
| [webpki-roots](https://crates.io/crates/webpki-roots/0.26.11) | 0.26.11 | CDLA-Permissive-2.0 |
| [webpki-roots](https://crates.io/crates/webpki-roots/1.0.9) | 1.0.9 | CDLA-Permissive-2.0 |
| [writeable](https://crates.io/crates/writeable/0.6.4) | 0.6.4 | Unicode-3.0 |
| [yoke](https://crates.io/crates/yoke/0.8.3) | 0.8.3 | Unicode-3.0 |
| [yoke-derive](https://crates.io/crates/yoke-derive/0.8.3) | 0.8.3 | Unicode-3.0 |
| [zerofrom](https://crates.io/crates/zerofrom/0.1.8) | 0.1.8 | Unicode-3.0 |
| [zerofrom-derive](https://crates.io/crates/zerofrom-derive/0.1.8) | 0.1.8 | Unicode-3.0 |
| [zerotrie](https://crates.io/crates/zerotrie/0.2.5) | 0.2.5 | Unicode-3.0 |
| [zerovec](https://crates.io/crates/zerovec/0.11.8) | 0.11.8 | Unicode-3.0 |
| [zerovec-derive](https://crates.io/crates/zerovec-derive/0.11.6) | 0.11.6 | Unicode-3.0 |

## 5. Components that are used but not distributed with Pierdesk

- **Microsoft Edge WebView2 Runtime** draws the settings and transfer windows. It is provided by Windows or installed by
  Microsoft; Pierdesk does not redistribute it.
- **Microsoft PowerToys, Mouse Without Borders** shares the keyboard and mouse between computers. It is a separate product
  that you install yourself; Pierdesk does not include it.

## 6. Original works

The Pierdesk logo and application icon, the interface text and the documentation are original works of the project and are
covered by the project license. The WeChat Pay and Alipay donation QR codes belong to the author.

## 7. License texts

- The license text of every crate in sections 3 and 4 is included in that crate's source package on crates.io (usually
  `LICENSE-MIT`, `LICENSE-APACHE` or similar). Where a crate ships a `NOTICE` file, it is in the same package.
- To read the licenses of exactly the versions used, run `cargo vendor` or `cargo fetch` in the repository root and open
  the files in each crate directory.
- If you cannot find a license text or want a combined file, open an issue and it will be added.

Corrections are welcome: if a component is missing or a license is described wrongly, please open an issue.
