# Symbolize logs

The [`ffx debug symbolize`][ffx-debug-symbolize] command processes logs
into symbolized format (that is, human-readable stack traces).

* [Concepts](#concepts)
* [Symbolize logs from an external source](#symbolize-external-logs)
* [Pass options and extra symbolizer flags](#extra-symbolizer-flags)

## Concepts {:#concepts}

The example below shows symbolized logs from a Fuchsia device:

```none {:.devsite-disable-click-to-copy}
$ ffx log --kernel
...
[326.124][klog][klog][I]    #0    0x0000428d2319f74b in abort() ../../zircon/third_party/ulib/musl/src/exit/abort.c:7 <libc.so>+0x6474b sp 0x27c83188f70
[326.124][klog][klog][I]    #1    0x000001cb1faef047 in main() src/hello_world/hello_world.cc:9 <<VMO#113249=blob-30a652b1>>+0x2047 sp 0x27c83188f80
[326.124][klog][klog][I]    #2    0x0000428d2319eef2 in start_main(const start_params*) ../../zircon/third_party/ulib/musl/src/env/__libc_start_main.c:140 <libc.so>+0x63ef2 sp 0x27c83188fa0
[326.124][klog][klog][I]    #3    0x0000428d2319f145 in __libc_start_main(zx_handle_t, int (*)(int, char**, char**)) ../../zircon/third_party/ulib/musl/src/env/__libc_start_main.c:215 <libc.so>+0x64145 sp 0x27c83188ff0
[326.124][klog][klog][I]    #4    0x000001cb1faef011 in _start(zx_handle_t) ../../zircon/system/ulib/c/Scrt1.cc:7 <<VMO#113249=blob-30a652b1>>+0x2011 sp 0x4046d0234fe0
[326.124][klog][klog][I]    #5    0x0000000000000000 is not covered by any module sp 0x4046d0234ff0
```

With symbolized logs, you can read the file names and line
numbers of the source code (for example,
`in main() src/hello_world/hello_world.cc:9`) where an error has occurred.

By default, [`ffx log`][ffx-log] automatically prints symbolized
logs when debug symbols are registered in your environment. (To turn off this
feature, run `ffx log --symbolize off`.) The `ffx debug symbolize` command
(which `ffx log` uses in the background) takes logs with symbolization markups
as input and generates symbolized logs as output. This process works as long as
the debug symbols corresponding to such markups are registered in your
environment (see [Register debug symbols][register-debug-symbols]).

## Symbolize logs from an external source {:#symbolize-external-logs}

During development, `ffx log` automatically processes registered debug
symbols and prints symbolized logs, so you may never need to manually process
logs into symbolized format in your environment. But in some cases, you may
obtain a copy of raw logs from an external source (for instance, output from
a Fuchsia builder). If you want to process such logs into symbolized format,
you can pass the logs as input to the `ffx debug symbolize` command.

To manually process logs into symbolized format, do the following:

Note: Symbolizing logs from an external source requires that the corresponding
debug symbols are registered in your environment or available online
(see [Register debug symbols][register-debug-symbols]).

1. Start the symbolize tool:

   ```posix-terminal
   ffx debug symbolize
   ```

   The terminal now waits for input.

2. Copy raw logs containing symbolization markups.
3. Paste them into the `ffx debug symbolize` terminal.

   The tool prints the logs in symbolized format.

## Pass options and extra symbolizer flags {:#extra-symbolizer-flags}

`ffx debug symbolize` supports built-in flags as well as forwarding additional
options directly to the underlying [`symbolizer`][symbolizer-reference] host
tool.

* [Built-in options](#built-in-options)
* [Extra symbolizer arguments](#extra-symbolizer-arguments)

### Built-in options {:#built-in-options}

* `--no-prettify`: Disables backtrace prettifying. By default,
  `ffx debug symbolize` passes `--prettify-backtrace` and `--omit-module-lines`
  to the symbolizer. Specifying `--no-prettify` omits these flags so raw frame
  lines and module mappings are printed.

### Extra symbolizer arguments {:#extra-symbolizer-arguments}

Any arguments starting with `-` intended for the underlying
[`symbolizer`][symbolizer-reference] tool must be specified after a `--`
separator:

```posix-terminal
ffx debug symbolize [options] -- [symbolizer_args...]
```

Common extra flags passed to the symbolizer include:

* `-s <path>`, `--symbol-path=<path>`: Adds a directory or ELF file to the
  symbol search path.
* `--build-id-dir=<path>`: Adds a `.build-id`-structured directory to the symbol
  search path.
* `--ids-txt=<path>`: Adds an `ids.txt` mapping file (`<build ID> <file path>`)
  to the symbol search path.
* `--symbol-index=<path>`: Specifies an explicit `symbol-index` configuration
  file (defaults to `~/.fuchsia/debug/symbol-index`).
* `--symbol-cache=<path>`: Specifies a custom directory for downloaded symbols
  cache (defaults to `~/.fuchsia/debug/symbol-cache`).
* `--symbol-server=<url>`: Adds a private symbol server URL (authentication
  requires running `gcloud auth application-default login`).
* `--public-symbol-server=<url>`: Adds an unauthenticated public symbol server
  URL.
* `--dumpfile-output=<path>`: Writes structured JSON dumpfile output to the
  given file path.
* `--verbose`: Enables `DEBUG`-level logging to `stderr`.

For example, to pass an explicit symbol path and enable verbose output:

```posix-terminal
ffx debug symbolize -- -s /path/to/symbols --verbose
```

To display usage information for the underlying `symbolizer` host tool:

```posix-terminal
ffx debug symbolize -- --help
```

<!-- Reference links -->

[ffx-debug-symbolize]: https://fuchsia.dev/reference/tools/sdk/ffx#symbolize
[ffx-log]: https://fuchsia.dev/reference/tools/sdk/ffx#ffx_log
[register-debug-symbols]: /docs/development/tools/ffx/workflows/register-debug-symbols.md
[symbolizer-reference]: /reference/tools/sdk/symbolizer.md
