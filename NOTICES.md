# Third-Party Licenses

DBDelve redistributes the following third-party font families, embedded within the binary through the `guic-gpui-assets` crate and registered at startup, and the theme files below.

## IBM Plex Sans

- **Copyright:** Copyright © 2017 IBM Corp. with Reserved Font Name "Plex"
- **License:** SIL Open Font License 1.1
- **Reserved Font Name:** "Plex"
- **Upstream:** https://github.com/IBM/plex
- **Full License Text:** [licenses/OFL-1.1.txt](licenses/OFL-1.1.txt)

## Lilex

- **Copyright:** Copyright 2019 The Lilex Project Authors (https://github.com/mishamyrt/Lilex)
- **License:** SIL Open Font License 1.1
- **Upstream:** https://github.com/mishamyrt/Lilex
- **Full License Text:** [licenses/OFL-1.1.txt](licenses/OFL-1.1.txt)

The full text of the SIL Open Font License 1.1 is included in [licenses/OFL-1.1.txt](licenses/OFL-1.1.txt).

## Zed's default themes

Zed's Ayu, Gruvbox and One theme files, from [`assets/themes`](https://github.com/zed-industries/zed/tree/main/assets/themes) in the Zed repository, are embedded within the binary for the Zed theme import ([assets/themes/zed](assets/themes/zed)). Each family carries its own licence there, reproduced here.

### Ayu

- **Copyright:** Copyright (c) 2016 Ike Ku
- **License:** MIT
- **Upstream:** https://github.com/dempfi/ayu
- **Full License Text:** [licenses/MIT-ayu.txt](licenses/MIT-ayu.txt)

### Gruvbox

- **Copyright:** not named: Zed's licence file for the family keeps the MIT template's `<YEAR> <COPYRIGHT HOLDER>`
- **License:** MIT
- **Upstream:** https://github.com/morhetz/gruvbox
- **Full License Text:** [licenses/MIT-gruvbox.txt](licenses/MIT-gruvbox.txt)

### One

- **Copyright:** Copyright (c) 2014 GitHub Inc.
- **License:** MIT
- **Upstream:** https://github.com/atom/atom/tree/master/packages/one-dark-ui
- **Full License Text:** [licenses/MIT-one.txt](licenses/MIT-one.txt)

## unixODBC

Statically linked into the ODBC bridge (`dbdelve-odbc`) that the macOS app bundle carries, through the `unix-odbc` crate. Linux builds use the system's `libodbc.so.2` and Windows builds the system's `odbc32`, so neither redistributes it.

- **Copyright:** Copyright (c) 1999 Nick Gorham, on code based on work by Peter Harvey; unixODBC's `AUTHORS` file names both as its authors, with a list of further contributors.
- **License:** LGPL-2.1-or-later
- **Exact source:** the [`unix-odbc`](https://crates.io/crates/unix-odbc) crate, version 0.1.4 (the version in `Cargo.lock`), which vendors unixODBC release 2.3.14 under its `unixODBC/` directory. The crate adds its own `config.h` and an `ltdl.h` that maps `lt_*` onto `dlopen`; neither comes from upstream. Upstream: https://www.unixodbc.org
- **Route:** LGPL-2.1 section 6(a). The corresponding source is available as above, together with `plugins/odbc/` in this repository at the tag `v<version>` of the release, which holds everything needed to relink the bridge against a modified unixODBC.
- **Relinking:** at the release's tag `v<version>`, point `[patch.crates-io]` in `Cargo.toml` at a modified copy of the `unix-odbc` crate, run `cargo build --release -p dbdelve-plugin-odbc`, and replace `Contents/Helpers/dbdelve-odbc` in the app bundle with the `target/release/dbdelve-odbc` it produces. Then sign again, the helper first and the bundle second, as `dev/bundle.sh` does: replacing the helper breaks the bundle's seal.
- **Full License Text:** [licenses/LGPL-2.1.txt](licenses/LGPL-2.1.txt)
