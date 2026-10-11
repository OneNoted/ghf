# ghf

Count code in a GitHub repository without cloning it: per-language code, comments and blanks, split into production, tests, docs, examples, fixtures, vendor and other. Counting is done by [tokei](https://github.com/XAMPPRocky/tokei).

    ghf BurntSushi/ripgrep
    ghf https://github.com/sharkdp/fd/tree/v10.2.0 --json

Binaries for Linux x86_64 and aarch64 (glibc 2.35+) are on the [releases page](https://github.com/OneNoted/ghf/releases); `ghf --help` explains targets, categories and authentication. MIT licensed.
