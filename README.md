# vmod_fileserver

Serve files directly from Varnish, no external backend needed!

The full API is documented in [API.md](API.md).

## Version matching

| vmod-fileserver | varnish |
|:----------------|:-------:|
| 0.1.0           |   9.0   |
| 0.0.12          |   9.0   |
| 0.0.11          |   9.0   |
| 0.0.10          |   8.0   |
| 0.0.9           |   7.7.1 |
| 0.0.8           |   7.7   |
| 0.0.7           |   7.6   |
| 0.0.6           |   7.5   |
| 0.0.5           |   7.4   |
| 0.0.3 -> 0.0.4  |   7.3   |
| 0.0.1 -> 0.0.2  |   7.2   |

## VCL Examples

``` vcl
import fileserver;

backend default none;

sub vcl_init {
	new www = fileserver.root("/var/www/html");
}

sub vcl_recv {
	set req.backend_hint = www.backend();
}
```

## Requirements

You'll need:
- `cargo` (and the accompanying `rust` package)
- `clang`
- the `varnish` 7.3 development libraries/headers ([depends on the `varnish` crate you are using](https://github.com/gquintard/varnish-rs#versions))

## Build and test

With `cargo` only:

``` bash
cargo build --release
cargo test --release
```

The vmod file will be found at `target/release/libvmod_fileserver.so`.

Alternatively, if you have `jq` and `rst2man`, you can use `build.sh`

``` bash
./build.sh [OUTDIR]
```

This will place the `so` file as well as the generated documentation in the `OUT` directory (or in the current directory if `OUT` wasn't specified).
