//! Send a document to the printer using one of supported backends.
//!
//! * [`ipp_client`] forwards over IPP.
//! * [`local`] uses CUPS, the Windows print spooler, SumatraPDF, or a
//!   custom command.
//!
//! [`Sink::detect`] builds either of them from a spec or the default for the
//! platform.

pub mod local;

#[cfg(feature = "ipp-client")]
pub mod ipp_client;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use local::{Local, LocalJob, Printer};

/// Local backends are killed by timeout; IPP requests are limited on HTTP level.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Retry policy. Only for IPP backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    pub attempts: u32,
    pub initial_delay: Duration,
    /// Delay doubles after each attempt, capped by `max_delay`.
    pub max_delay: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 4,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl Retry {
    pub const fn none() -> Self {
        Retry {
            attempts: 1,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    pub fn delay_before(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let doublings = attempt - 2;
        self.initial_delay
            .saturating_mul(2u32.saturating_pow(doublings.min(31)))
            .min(self.max_delay)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The job was rejected by the printer.
    #[cfg(feature = "ipp-client")]
    #[error("the printer rejected the job: {status}{}", if .detail.is_empty() { String::new() } else { format!(" ({detail})", detail = .detail) })]
    Rejected {
        status: ipp::model::StatusCode,
        detail: String,
    },
    /// The printer took the request and never answered it.
    #[cfg(feature = "ipp-client")]
    #[error("{uri} did not answer within {timeout:?}")]
    Timeout { uri: String, timeout: Duration },
    /// The job could not be sent at all.
    #[error("{0}")]
    Backend(String),
    /// This build, platform or printer cannot do what was asked.
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn is_retryable(&self) -> bool {
        #[cfg(feature = "ipp-client")]
        if let Error::Rejected { status, .. } = self {
            use ipp::model::StatusCode;
            return matches!(
                status,
                StatusCode::ServerErrorBusy
                    | StatusCode::ServerErrorTemporaryError
                    | StatusCode::ServerErrorNotAcceptingJobs
            );
        }
        false
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A document on its way to a printer.
#[derive(Debug, Clone)]
pub struct Job<'a> {
    pub data: &'a [u8],
    /// MIME media type of `data`.
    pub format: &'a str,
    /// A title for the printer's queue.
    pub name: &'a str,
    /// Who to attribute the job to, where the backend can say.
    pub user: Option<&'a str>,
    pub copies: u32,
    /// A PWG media name, e.g. `iso_a4_210x297mm`.
    pub media: Option<&'a str>,
    pub sides: &'a str,
}

impl<'a> Job<'a> {
    /// A one-sided, single-copy job with no preferences.
    pub fn new(data: &'a [u8], format: &'a str, name: &'a str) -> Self {
        Job {
            data,
            format,
            name,
            user: None,
            copies: 1,
            media: None,
            sides: "one-sided",
        }
    }
}

pub enum Sink {
    /// Forward to another IPP printer.
    #[cfg(feature = "ipp-client")]
    Ipp(ipp_client::IppSink),
    /// Hand to the operating system. The document is written to a temporary
    /// file first.
    Local {
        backend: Local,
        printer: Option<String>,
        timeout: Duration,
    },
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

impl Sink {
    /// Builds a sink from a short textual specification, or attempts to auto-detect one.
    pub fn detect(spec: Option<&str>, printer: Option<&str>, timeout: Duration) -> Result<Sink> {
        let spec = spec.map(str::trim).filter(|s| !s.is_empty());
        let printer = printer.filter(|p| !p.is_empty());

        let (name, value) = match spec {
            Some(spec) => match spec.split_once(':') {
                Some((name, value)) => (name, Some(value)),
                None => (spec, None),
            },
            None => ("auto", None),
        };
        let name = name.to_ascii_lowercase();

        // A URI is told apart by its scheme, which only `ipp://` and friends
        // can have: every other spec is a bare name or `name:value`.
        if value.is_some() && matches!(name.as_str(), "ipp" | "ipps" | "http" | "https") {
            return ipp_sink(spec.unwrap_or_default(), printer, timeout);
        }

        let backend = match name.as_str() {
            "auto" => Local::default_backend()?,
            "lp" => Local::Lp,
            "lpr" => Local::Lpr,
            "winspooler" => win_spooler()?,
            "sumatra" => {
                Local::Sumatra(helper(value, "SumatraPDF", "sumatra", local::find_sumatra)?)
            }
            "command" => {
                let template = value.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
                    Error::Unsupported(
                        "a command spec needs a template, as in \
                         command:lp -d {printer} {file}"
                            .into(),
                    )
                })?;
                Local::Command(template.to_string())
            }
            "dry-run" => Local::DryRun,
            other => {
                return Err(Error::Unsupported(format!(
                    "unknown destination {other:?}; use auto, lp, lpr, winspooler, \
                     sumatra[:path], command:<template>, dry-run, or an ipp:// URI"
                )));
            }
        };

        Ok(Sink::Local {
            backend,
            printer: printer.map(str::to_string),
            timeout,
        })
    }

    /// Sets how often a busy printer is asked again.
    ///
    /// Only the IPP backend retries; for a local backend this does nothing.
    pub fn with_retry(self, retry: Retry) -> Self {
        #[cfg(feature = "ipp-client")]
        {
            let mut sink = self;
            if let Sink::Ipp(ipp) = &mut sink {
                ipp.set_retry(retry);
            }
            sink
        }
        #[cfg(not(feature = "ipp-client"))]
        {
            let _ = retry;
            self
        }
    }

    /// A description for a startup log line.
    pub fn label(&self) -> String {
        match self {
            #[cfg(feature = "ipp-client")]
            Sink::Ipp(sink) => format!("IPP to {}", sink.uri()),
            Sink::Local {
                backend, printer, ..
            } => match printer {
                Some(name) => format!("{} to {name}", backend.label()),
                None => backend.label(),
            },
        }
    }

    /// Checks that the destination can take `format` if supported.
    pub async fn check(&self, format: &str) -> Result<()> {
        // Only the IPP backend can answer this.
        #[cfg(not(feature = "ipp-client"))]
        let _ = format;

        match self {
            #[cfg(feature = "ipp-client")]
            Sink::Ipp(sink) => {
                let formats = sink.document_formats().await?;
                if ipp_client::accepts(&formats, format) {
                    Ok(())
                } else {
                    Err(ipp_client::unsupported_format(sink.uri(), format, &formats))
                }
            }
            // Not supported for local backends.
            Sink::Local { .. } => Ok(()),
        }
    }

    /// The printers this destination can send a job to.
    pub async fn printers(&self) -> Result<Vec<Printer>> {
        match self {
            #[cfg(feature = "ipp-client")]
            Sink::Ipp(_) => Ok(Vec::new()),
            Sink::Local { backend, .. } => {
                let backend = backend.clone();
                blocking(move || backend.printers()).await
            }
        }
    }

    pub async fn print(&self, job: &Job<'_>) -> Result<()> {
        match self {
            #[cfg(feature = "ipp-client")]
            Sink::Ipp(sink) => {
                let id = sink.print(job).await?;
                if let Some(id) = id {
                    tracing::debug!(downstream_job = id, "forwarded");
                }
                Ok(())
            }
            Sink::Local {
                backend,
                printer,
                timeout,
            } => {
                let file = spool(job)?;
                let backend = backend.clone();
                let job = local::LocalJob::new(job, printer.as_deref(), *timeout);

                blocking(move || backend.print_file(file.path(), &job)).await
            }
        }
    }
}

/// Runs a local backend in a separate thread.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    #[cfg(feature = "ipp-client")]
    {
        tokio::task::spawn_blocking(call)
            .await
            .map_err(|e| Error::Backend(format!("the print task failed: {e}")))?
    }
    #[cfg(not(feature = "ipp-client"))]
    {
        call()
    }
}

/// Takes the path a spec gave for a helper application, or looks for it.
fn helper(
    value: Option<&str>,
    title: &str,
    spec: &str,
    find: impl Fn() -> Option<PathBuf>,
) -> Result<PathBuf> {
    match value.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => find().ok_or_else(|| {
            Error::Unsupported(format!(
                "{title} was not found; name it with {spec}:<path> or pass a print command"
            ))
        }),
    }
}

/// The Windows print spooler, if this platform and build have one.
fn win_spooler() -> Result<Local> {
    if cfg!(all(windows, feature = "windows-spooler")) {
        Ok(Local::WinSpooler)
    } else {
        Err(local::spooler_unavailable())
    }
}

#[cfg(feature = "ipp-client")]
fn ipp_sink(uri: &str, printer: Option<&str>, timeout: Duration) -> Result<Sink> {
    if let Some(printer) = printer {
        return Err(Error::Unsupported(format!(
            "an IPP URI already names its queue, so it cannot also be pointed at {printer:?}"
        )));
    }
    let parsed = uri
        .parse::<ipp::prelude::Uri>()
        .map_err(|e| Error::Unsupported(format!("{uri:?} is not a usable IPP URI: {e}")))?;
    Ok(Sink::Ipp(ipp_client::IppSink::new(parsed, timeout)))
}

#[cfg(not(feature = "ipp-client"))]
fn ipp_sink(uri: &str, _printer: Option<&str>, _timeout: Duration) -> Result<Sink> {
    Err(Error::Unsupported(format!(
        "cannot print to {uri:?}: the ipp-client feature is off in this build"
    )))
}

/// A temporary file that removes itself.
struct Spooled {
    path: std::path::PathBuf,
}

impl Spooled {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Spooled {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes a job to a temporary file for a backend that needs a path.
fn spool(job: &Job<'_>) -> Result<Spooled> {
    use std::io::Write as _;

    let extension = match job.format {
        "application/pdf" => "pdf",
        "image/jpeg" => "jpg",
        "image/pwg-raster" => "pwg",
        _ => "bin",
    };
    // A counter keeps two jobs in the same millisecond apart.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let path = std::env::temp_dir().join(format!(
        "print-sink-{}-{serial}.{extension}",
        std::process::id()
    ));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(job.data)?;
    file.sync_all()?;

    Ok(Spooled { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spooling_writes_the_document_and_cleans_up_after_itself() {
        let job = Job::new(b"%PDF-1.7 body", "application/pdf", "test");
        let path = {
            let spooled = spool(&job).unwrap();
            let path = spooled.path().to_path_buf();
            assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.7 body");
            assert_eq!(path.extension().unwrap(), "pdf");
            path
        };
        assert!(!path.exists(), "the spool file outlived its job");
    }

    #[test]
    fn the_spool_extension_follows_the_format() {
        for (format, extension) in [
            ("image/jpeg", "jpg"),
            ("image/pwg-raster", "pwg"),
            ("application/vnd.nonsense", "bin"),
        ] {
            let job = Job::new(b"x", format, "test");
            let spooled = spool(&job).unwrap();
            assert_eq!(spooled.path().extension().unwrap(), extension);
        }
    }

    #[test]
    fn a_spec_names_a_local_backend() {
        for (spec, expected) in [
            ("lp", Local::Lp),
            ("LPR", Local::Lpr),
            ("dry-run", Local::DryRun),
            ("DRY-RUN", Local::DryRun),
        ] {
            let sink = Sink::detect(Some(spec), None, DEFAULT_TIMEOUT).unwrap();
            assert!(
                matches!(&sink, Sink::Local { backend, .. } if *backend == expected),
                "{spec} gave {sink:?}"
            );
        }
        assert!(Sink::detect(Some("nonsense"), None, DEFAULT_TIMEOUT).is_err());

        // Each backend has exactly one spelling; these redundant ones were
        // dropped, and nothing should quietly accept them again.
        for dropped in ["default", "none", "dryrun", "win-spooler"] {
            assert!(
                Sink::detect(Some(dropped), None, DEFAULT_TIMEOUT).is_err(),
                "{dropped} is still accepted"
            );
        }
    }

    #[test]
    fn a_printer_name_rides_along_with_a_local_backend() {
        let sink = Sink::detect(Some("lp"), Some("HP"), DEFAULT_TIMEOUT).unwrap();
        assert!(matches!(&sink, Sink::Local { printer: Some(p), .. } if p == "HP"));
        assert!(sink.label().contains("HP"));

        // An empty name is no name at all.
        let sink = Sink::detect(Some("lp"), Some(""), DEFAULT_TIMEOUT).unwrap();
        assert!(matches!(sink, Sink::Local { printer: None, .. }));
    }

    #[test]
    fn a_command_spec_keeps_the_rest_of_the_line() {
        let sink = Sink::detect(
            Some("command:lp -d {printer} -o media=A4 {file}"),
            None,
            DEFAULT_TIMEOUT,
        )
        .unwrap();
        assert!(
            matches!(&sink, Sink::Local { backend: Local::Command(t), .. }
                if t == "lp -d {printer} -o media=A4 {file}"),
            "{sink:?}"
        );
        assert!(Sink::detect(Some("command:"), None, DEFAULT_TIMEOUT).is_err());
    }

    #[test]
    fn an_explicit_helper_path_is_taken_as_given() {
        let sink = Sink::detect(
            Some(r"sumatra:C:\Tools\SumatraPDF.exe"),
            None,
            DEFAULT_TIMEOUT,
        )
        .unwrap();
        assert!(
            matches!(&sink, Sink::Local { backend: Local::Sumatra(exe), .. }
                if exe == Path::new(r"C:\Tools\SumatraPDF.exe")),
            "{sink:?}"
        );
    }

    #[test]
    fn the_windows_spooler_is_only_offered_where_it_works() {
        let chosen = Sink::detect(Some("winspooler"), None, DEFAULT_TIMEOUT);
        if cfg!(all(windows, feature = "windows-spooler")) {
            assert!(matches!(
                chosen.unwrap(),
                Sink::Local {
                    backend: Local::WinSpooler,
                    ..
                }
            ));
        } else {
            assert!(chosen.is_err());
        }
    }

    #[test]
    fn no_spec_at_all_means_the_default_mode() {
        let bare = Sink::detect(None, None, DEFAULT_TIMEOUT);
        let named = Sink::detect(Some("auto"), None, DEFAULT_TIMEOUT);
        let blank = Sink::detect(Some("   "), None, DEFAULT_TIMEOUT);
        for sink in [&bare, &named, &blank] {
            match sink {
                Ok(Sink::Local { backend, .. }) => {
                    assert_eq!(*backend, *Local::default_backend().as_ref().unwrap())
                }
                Err(error) => assert!(error.to_string().contains("no print"), "{error}"),
                #[cfg(feature = "ipp-client")]
                Ok(other) => panic!("the default mode must stay local, got {other:?}"),
            }
        }
    }

    #[cfg(feature = "ipp-client")]
    #[test]
    fn an_ipp_uri_becomes_an_ipp_sink() {
        let sink =
            Sink::detect(Some("ipp://printer.lan/ipp/print"), None, DEFAULT_TIMEOUT).unwrap();
        assert!(matches!(sink, Sink::Ipp(_)));
        assert!(sink.label().contains("printer.lan"), "{sink:?}");

        // The queue is in the URI, so a second name would be ambiguous.
        assert!(
            Sink::detect(
                Some("ipp://printer.lan/ipp/print"),
                Some("HP"),
                DEFAULT_TIMEOUT
            )
            .is_err()
        );
    }

    #[cfg(not(feature = "ipp-client"))]
    #[test]
    fn an_ipp_uri_without_the_feature_says_so() {
        let error = Sink::detect(Some("ipp://printer.lan/ipp/print"), None, DEFAULT_TIMEOUT)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ipp-client"), "{error}");
    }

    #[test]
    fn the_wait_between_attempts_doubles_up_to_the_ceiling() {
        let retry = Retry {
            attempts: 6,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
        };
        // The first attempt is immediate; the rest back off.
        assert_eq!(retry.delay_before(1), Duration::ZERO);
        assert_eq!(retry.delay_before(2), Duration::from_secs(2));
        assert_eq!(retry.delay_before(3), Duration::from_secs(4));
        assert_eq!(retry.delay_before(4), Duration::from_secs(8));
        // Capped, not doubled for ever.
        assert_eq!(retry.delay_before(5), Duration::from_secs(10));
        assert_eq!(retry.delay_before(99), Duration::from_secs(10));
    }

    #[test]
    fn the_default_schedule_is_over_in_well_under_a_minute() {
        // A job held too long stops being worth printing, and the queue
        // behind it is waiting.
        let retry = Retry::default();
        let total: Duration = (1..=retry.attempts).map(|n| retry.delay_before(n)).sum();
        assert!(total <= Duration::from_secs(20), "{total:?}");
        assert_eq!(Retry::none().attempts, 1);
    }

    #[cfg(feature = "ipp-client")]
    #[test]
    fn only_the_printers_ask_me_later_statuses_are_worth_another_go() {
        use ipp::model::StatusCode;

        let rejected = |status| Error::Rejected {
            status,
            detail: String::new(),
        };
        assert!(rejected(StatusCode::ServerErrorBusy).is_retryable());
        assert!(rejected(StatusCode::ServerErrorTemporaryError).is_retryable());
        assert!(rejected(StatusCode::ServerErrorNotAcceptingJobs).is_retryable());

        // The document will be just as unsupported in two seconds.
        assert!(!rejected(StatusCode::ClientErrorDocumentFormatNotSupported).is_retryable());
        assert!(!rejected(StatusCode::ServerErrorInternalError).is_retryable());
        assert!(!Error::Backend("no route to host".into()).is_retryable());
    }

    #[cfg(feature = "ipp-client")]
    #[test]
    fn an_ipp_sink_keeps_the_timeout_it_was_given() {
        // Without this the IPP backend waits for ever on a printer that
        // accepts the connection and then says nothing.
        let sink = Sink::detect(
            Some("ipp://printer.lan/ipp/print"),
            None,
            Duration::from_secs(7),
        )
        .unwrap();
        match sink {
            Sink::Ipp(sink) => assert_eq!(sink.timeout(), Duration::from_secs(7)),
            other => panic!("expected an IPP sink, got {other:?}"),
        }
    }

    #[cfg(feature = "ipp-client")]
    #[test]
    fn the_retry_schedule_can_be_set_on_a_built_sink() {
        let sink = Sink::detect(Some("ipp://printer.lan/ipp/print"), None, DEFAULT_TIMEOUT)
            .unwrap()
            .with_retry(Retry::none());
        match sink {
            Sink::Ipp(sink) => assert_eq!(sink.retry(), Retry::none()),
            other => panic!("expected an IPP sink, got {other:?}"),
        }

        // A local backend has no use for it, and must not choke on it.
        let local = Sink::detect(Some("dry-run"), None, DEFAULT_TIMEOUT)
            .unwrap()
            .with_retry(Retry::none());
        assert!(matches!(local, Sink::Local { .. }));
    }

    #[cfg(feature = "ipp-client")]
    #[tokio::test]
    async fn an_ipp_sink_has_no_printers_to_offer() {
        // The queue is in the URI, and IPP gives no way to browse for more.
        let sink =
            Sink::detect(Some("ipp://printer.lan/ipp/print"), None, DEFAULT_TIMEOUT).unwrap();
        assert!(sink.printers().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_local_sink_lists_through_its_backend() {
        let sink = Sink::Local {
            backend: Local::DryRun,
            printer: None,
            timeout: DEFAULT_TIMEOUT,
        };
        assert!(sink.printers().await.unwrap().is_empty());

        // A backend that cannot be asked reports that, rather than nothing.
        let sink = Sink::detect(Some("command:mycmd {file}"), None, DEFAULT_TIMEOUT).unwrap();
        assert!(sink.printers().await.is_err());
    }

    #[tokio::test]
    async fn a_dry_run_accepts_everything_and_says_so() {
        let sink = Sink::Local {
            backend: Local::DryRun,
            printer: None,
            timeout: DEFAULT_TIMEOUT,
        };
        assert!(sink.check("application/pdf").await.is_ok());
        assert!(
            sink.print(&Job::new(b"%PDF-1.7", "application/pdf", "test"))
                .await
                .is_ok()
        );
        assert!(sink.label().contains("dry run"));
    }
}
