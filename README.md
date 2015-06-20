# Gearbox - A simple bittorrent client written in Rust

Gearbox is a simple command-line bittorrent client written in Rust. It is
currently very much a work-in-progress and I wouldn't recommend actually using
it. It may have all sorts of horrible bugs that format you hard drive or
frighten your cat. That being said:

## Platforms

Gearbox supports Linux and OS X.

## Installation

To build Gearbox, you will need a recent nightly version of the rustc compiler
and the Cargo build tool. You can find these
[here](http://www.rust-lang.org/install.html).

Once you have those installed, you can just run `cargo build` (or
`cargo build --release` if you want to compile with optimisations) to build the
executable.

## Usage 

Just run

    path/to/gearbox path/to/torrent_file.torrent
