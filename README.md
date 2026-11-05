# universal-print

A small shim for printing documents.

```rust
use universal_print::{Job, Sink, DEFAULT_TIMEOUT};

# async fn example() -> universal_print::Result<()> {
// `None` means the default mode.
let sink = Sink::detect(None, None, DEFAULT_TIMEOUT)?;

// Fails at startup rather than one job at a time, where it can be asked.
sink.check("application/pdf").await?;

// List available queues for supported backends.
for printer in sink.printers().await? {
    println!("{}{}", printer.name, if printer.is_default { " (default)" } else { "" });
}

sink.print(&Job::new(b"%PDF-1.7 ...", "application/pdf", "my job")).await?;
# Ok(())
# }
```

## Backends

| Class | Description | Supported OS | Spec |
|---|---|---|---|
| `Sink::Ipp` | Sends a job to a printer supporting [Internet Printing Protocol][ipp] | Any | `ipp://..`, `ipps://..`, `http://..`, `https://..` |
| `Local::Lp` / `Local::Lpr` | Invokes [`lp`][lp] (POSIX-compliant) or [`lpr`][lpr] (BSD-compatible) | Linux | `lp`, `lpr` |
| `Local::Sumatra` | Invokes [Sumatra PDF][sumatra] executable | Windows | `sumatra`, `sumatra:<path>` |
| `Local::WinSpooler` | [Windows Print Spooler][spoolsv.exe] | Windows | `winspooler` |
| `Local::Command` | Invokes command of your own; `{file}` and `{printer}` are substituted | Any | `command:<template>` |
| `Local::DryRun` | Does nothing | Any | `dry-run` |

[ipp]: https://www.pwg.org/ipp/ippguide.html
[lp]: https://man7.org/linux/man-pages/man1/lp.1.html
[lpr]: https://man7.org/linux/man-pages/man1/lpr.1.html
[spoolsv.exe]: https://learn.microsoft.com/en-us/windows/win32/printdocs/print-spooler
[sumatra]: https://www.sumatrapdfreader.org/download-free-pdf-viewer

## Build from spec and auto-detect

`Sink::detect(spec, printer, timeout)` builds a sink from a spec.

If `None` or `"auto"` is passed as `spec`, the backend is auto-detected:

* On Unix-based systems, it looks for `lp`, then `lpr`, then returns an error.
* On Windows-based systems, it looks for SumatraPDF, then tries to use `WinSpooler`.

`printer` names a queue for the local backends. Use `sink.printers()` to list them.

## Features

All features are enabled by default.

* `ipp-client` — the IPP backend
* `aws-lc-rs` — picks the TLS provider for it
* `windows-spooler` — `Local::WinSpooler` support via the `printers` crate

The local backends are synchronous and spawn at most one child process, so
they work from a plain thread as well as from an async runtime; `Sink` moves
them onto a blocking task for you when `ipp-client` is used.
