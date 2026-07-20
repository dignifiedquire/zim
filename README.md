# zim

> A rust library and cli tool to read and extract zim files.

## Build

```sh
> cargo build --release
```

## Split archives

Archives split into chunks (`data.zimaa`, `data.zimab`, ...) are read transparently. Name either
the archive or its first chunk:

```sh
> ./target/release/zim-info data.zim
> ./target/release/zim-info data.zimaa
```

## Usage with IPFS

To add a file `data.zim` to ipfs do the following.


```sh
> ./target/release/extract-zim --skip-link data.zim
> ipfs add -r out
> ipfs files cp /ipfs/<outhash> /
> ipfs files mv /<outhash> /data
> ./target/release/ipfs-link /data data.zim
```

and then execute all commands in `link.txt`


## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
