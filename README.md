# universal-print

A small shim for printing documents.

```rust
use universal_print::{Job, Sink, Local, DEFAULT_TIMEOUT};

# async fn example() -> universal_print::Result<()> {
let sink = Sink::Local {
    backend: Local::detect(None, None)?,
    printer: None,
    timeout: DEFAULT_TIMEOUT,
};

// Fails at startup rather than one job at a time, where it can be asked.
sink.check("application/pdf").await?;

sink.print(&Job::new(b"%PDF-1.7 ...", "application/pdf", "my job")).await?;
# Ok(())
# }
```

## Backends

| Class | Description | Supported OS |
|---|---|---|
| `Sink::Ipp` | Sends a job to a printer supporting [Internet Printing Protocol][ipp] | Any |
| `Local::Lp` / `Local::Lpr` | Invokes [`lp`][lp] (POSIX-compliant) or [`lpr`][lpr] (BSD-compatible) | Linux (default) |
| `Local::Native` | [Windows Print Spooler][spoolsv.exe] | Windows (default) |
| `Local::Sumatra` | Invokes [Sumatra PDF][sumatra] executable | Windows |
| `Local::Acrobat` | Invokes [Adobe Acrobat Reader][acrobat] or Acrobat Pro | Windows |
| `Local::Command` | Invokes command of your own; `{file}` and `{printer}` are substituted | Any |
| `Local::DryRun` | Does nothing | Any |

`Local::detect` picks one by name, or by what is installed.

[ipp]: https://www.pwg.org/ipp/ippguide.html
[lp]: https://man7.org/linux/man-pages/man1/lp.1.html
[lpr]: https://man7.org/linux/man-pages/man1/lpr.1.html
[spoolsv.exe]: https://learn.microsoft.com/en-us/windows/win32/printdocs/print-spooler
[sumatra]: https://www.sumatrapdfreader.org/download-free-pdf-viewer
[acrobat]: https://get.adobe.com/reader/otherversions

## Features

* `ipp-client` — the IPP backend
* `windows-spooler` — `Local::Native` support via the `printers` crate

The local backends are synchronous and spawn at most one child process, so
they work from a plain thread as well as from an async runtime; `Sink` moves
them onto a blocking task for you when `ipp-client` is used.
