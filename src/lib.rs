//! Send a document to the printer using one of supported backends.
//!
//! * [`ipp_client`] forwards over IPP.
//! * [`local`] uses CUPS, the Windows spooler, or a custom command.

pub mod local;

#[cfg(feature = "ipp-client")]
pub mod ipp_client;

use std::path::Path;
use std::time::Duration;

pub use local::Local;

/// How long a local print command may run before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The job was rejected by the printer.
    #[cfg(feature = "ipp-client")]
    #[error("the printer rejected the job: {status}{}", if .detail.is_empty() { String::new() } else { format!(" ({detail})", detail = .detail) })]
    Rejected {
        status: ipp::model::StatusCode,
        detail: String,
    },
    /// The job could not be sent at all.
    #[error("{0}")]
    Backend(String),
    /// This build, platform or printer cannot do what was asked.
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
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
                let printer = printer.clone();
                let timeout = *timeout;
                let copies = job.copies;
                let media = job.media.map(str::to_string);

                // The local backends block on a child process, so they must
                // not run on the async runtime's worker threads.
                #[cfg(feature = "ipp-client")]
                {
                    tokio::task::spawn_blocking(move || {
                        backend.print_file(
                            file.path(),
                            printer.as_deref(),
                            copies,
                            media.as_deref(),
                            timeout,
                        )
                    })
                    .await
                    .map_err(|e| Error::Backend(format!("the print task failed: {e}")))?
                }
                #[cfg(not(feature = "ipp-client"))]
                {
                    backend.print_file(
                        file.path(),
                        printer.as_deref(),
                        copies,
                        media.as_deref(),
                        timeout,
                    )
                }
            }
        }
    }
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
