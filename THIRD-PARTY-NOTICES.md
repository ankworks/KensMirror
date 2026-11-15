# Third-party notices

KensMirror is distributed under GPL-3.0-or-later. It is built on the libraries below,
each under its own license. The licenses shown are the ones declared in each crate's
`Cargo.toml` at the listed version.

| Crate | Version | License |
| --- | --- | --- |
| rust-airplay | 0.3.2 | GPL-3.0-or-later |
| rusty_aac | 2.0.2 | Apache-2.0 |
| cpal | 0.18 | Apache-2.0 |
| winit | 0.30 | Apache-2.0 |
| softbuffer | 0.4 | MIT OR Apache-2.0 |
| windows | 0.62 | MIT OR Apache-2.0 |
| tokio | 1 | MIT |
| log | 0.4 | MIT OR Apache-2.0 |
| getrandom | 0.4 | MIT OR Apache-2.0 |

These crates pull in further crates of their own. `Cargo.lock` lists every one with its
exact version; a tool such as `cargo license` or `cargo deny` prints the license of each.

rust-airplay carries its own third-party notices, including the origin of its pairing
and key-exchange code. Read the `THIRD_PARTY_NOTICES.md` file in that project before
redistributing a build.

Video decoding and the MP4, H.264 and AAC encoders used for recording are components of
Windows (Media Foundation). They are used through the operating system and are not
distributed with KensMirror.
