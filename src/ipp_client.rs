//! Send a document to the IPP printer.

use std::io::Cursor;
use std::time::Duration;

use ipp::attribute::{IppAttribute, IppAttributeName};
use ipp::error::IppError;
use ipp::model::{DelimiterTag, StatusCode};
use ipp::payload::IppPayload;
use ipp::prelude::{AsyncIppClient, IppOperationBuilder, Uri};
use ipp::value::{IppKeyword, IppValue};

use crate::{Error, Result, Retry};

/// A printer reachable over IPP.
pub struct IppSink {
    uri: Uri,
    /// How long one request may take, answer included.
    timeout: Duration,
    /// How often a busy printer is asked again.
    retry: Retry,
}

impl IppSink {
    pub fn new(uri: Uri, timeout: Duration) -> Self {
        IppSink {
            uri,
            timeout,
            retry: Retry::default(),
        }
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn retry(&self) -> Retry {
        self.retry
    }

    pub fn set_retry(&mut self, retry: Retry) {
        self.retry = retry;
    }

    /// A fresh client. The `ipp` client holds a connection pool, and a job
    /// may be minutes apart from the last, so one per operation keeps a stale
    /// pooled connection from failing to print.
    fn client(&self) -> AsyncIppClient {
        AsyncIppClient::builder(self.uri.clone())
            .request_timeout(self.timeout)
            .build()
    }

    /// Describes a request that never got an answer, saying plainly when the
    /// cause was the clock rather than the network.
    fn unanswered(&self, doing: &str, error: IppError) -> Error {
        if matches!(&error, IppError::AsyncClientError(e) if e.is_timeout()) {
            Error::Timeout {
                uri: self.uri.to_string(),
                timeout: self.timeout,
            }
        } else {
            Error::Backend(format!("could not {doing} {}: {error}", self.uri))
        }
    }

    /// Asks the printer what document formats it accepts.
    pub async fn document_formats(&self) -> Result<Vec<String>> {
        let operation = IppOperationBuilder::get_printer_attributes(self.uri.clone())
            .attribute(IppAttribute::DOCUMENT_FORMAT_SUPPORTED)
            .build()
            .map_err(|e| {
                Error::Backend(format!(
                    "could not build the attribute query for {}: {e}",
                    self.uri
                ))
            })?;

        let response = self
            .client()
            .send(operation)
            .await
            .map_err(|e| self.unanswered("reach", e))?;

        if !response.header().status_code().is_success() {
            return Err(Error::Backend(format!(
                "{} answered {}",
                self.uri,
                response.header().status_code()
            )));
        }

        let formats = response
            .attributes()
            .first_of(DelimiterTag::PrinterAttributes)
            .and_then(|group| group.get(IppAttribute::DOCUMENT_FORMAT_SUPPORTED))
            .map(|attribute| {
                attribute
                    .value()
                    .into_iter()
                    .filter_map(|value| match value {
                        IppValue::MimeMediaType(format) => Some(format.as_str().to_owned()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(formats)
    }

    /// Sends a document, returning the job id if it gave one.
    pub async fn print(&self, job: &crate::Job<'_>) -> Result<Option<i32>> {
        let attempts = self.retry.attempts.max(1);
        let mut attempt = 1;
        loop {
            match self.send(job).await {
                Err(error) if attempt < attempts && error.is_retryable() => {
                    let delay = self.retry.delay_before(attempt + 1);
                    tracing::warn!(
                        %error,
                        attempt,
                        attempts,
                        "the printer would not take the job; trying again in {delay:?}"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(self.explained(error).await),
                outcome => return outcome,
            }
        }
    }

    /// Adds what the printer says about itself to a rejection, so a log line
    /// can say why rather than only what.
    ///
    /// Costs one request, and only when a job is being given up on.
    async fn explained(&self, error: Error) -> Error {
        let Error::Rejected { status, detail } = error else {
            return error;
        };

        let reasons = self.state_reasons().await.join(", ");
        let detail = match (detail.is_empty(), reasons.is_empty()) {
            (_, true) => detail,
            (true, false) => reasons,
            (false, false) => format!("{detail}; {reasons}"),
        };
        Error::Rejected { status, detail }
    }

    /// What the printer says is wrong with it, such as `media-empty` or
    /// `media-jam`. Best effort: a printer that will not say leaves the
    /// rejection exactly as it was.
    async fn state_reasons(&self) -> Vec<String> {
        let Ok(operation) = IppOperationBuilder::get_printer_attributes(self.uri.clone())
            .attribute(IppAttribute::PRINTER_STATE_REASONS)
            .build()
        else {
            return Vec::new();
        };
        let Ok(response) = self.client().send(operation).await else {
            return Vec::new();
        };
        if !response.header().status_code().is_success() {
            return Vec::new();
        }

        response
            .attributes()
            .first_of(DelimiterTag::PrinterAttributes)
            .and_then(|group| group.get(IppAttribute::PRINTER_STATE_REASONS))
            .map(|attribute| {
                attribute
                    .value()
                    .into_iter()
                    .filter_map(|value| match value {
                        IppValue::Keyword(reason) => Some(reason.as_str().to_owned()),
                        _ => None,
                    })
                    // "none" is the printer saying it is fine.
                    .filter(|reason| reason != "none")
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One attempt at sending the document.
    async fn send(&self, job: &crate::Job<'_>) -> Result<Option<i32>> {
        let payload = IppPayload::new(Cursor::new(job.data.to_vec()));

        let mut operation = IppOperationBuilder::print_job(self.uri.clone(), payload)
            .job_title(job.name)
            .document_format(job.format);
        if let Some(user) = job.user {
            operation = operation.user_name(user);
        }
        if job.copies > 1 {
            operation = operation.attribute(IppAttribute::new(
                IppAttributeName::COPIES,
                IppValue::Integer(job.copies as i32),
            ));
        }
        if let Some(media) = job.media {
            operation = operation.attribute(keyword(IppAttributeName::MEDIA, media)?);
        }
        if job.sides != "one-sided" {
            operation = operation.attribute(keyword(IppAttributeName::SIDES, job.sides)?);
        }

        let operation = operation.build().map_err(|e| {
            Error::Backend(format!("could not build the job for {}: {e}", self.uri))
        })?;

        let response = self
            .client()
            .send(operation)
            .await
            .map_err(|e| self.unanswered("send the job to", e))?;

        let status = response.header().status_code();
        if !status.is_success() {
            // A printer may explain itself in either message, or neither.
            let detail = [
                IppAttribute::STATUS_MESSAGE,
                IppAttribute::DETAILED_STATUS_MESSAGE,
            ]
            .into_iter()
            .filter_map(|name| {
                response
                    .attributes()
                    .first_of(DelimiterTag::OperationAttributes)
                    .and_then(|group| group.get(name))
                    .map(|a| a.value().to_string())
            })
            .filter(|message| !message.trim().is_empty())
            .collect::<Vec<_>>()
            .join("; ");
            return Err(Error::Rejected { status, detail });
        }

        let id = response
            .attributes()
            .first_of(DelimiterTag::JobAttributes)
            .and_then(|group| group.get(IppAttribute::JOB_ID))
            .and_then(|attribute| match attribute.value() {
                IppValue::Integer(id) => Some(*id),
                _ => None,
            });

        Ok(id)
    }
}

/// Whether a list of formats, as reported by a printer, includes one.
///
/// Allows printers that report no formats.
pub fn accepts(formats: &[String], wanted: &str) -> bool {
    formats.is_empty() || formats.iter().any(|f| f.eq_ignore_ascii_case(wanted))
}

/// Builds the error for incompatible format.
pub fn unsupported_format(uri: &Uri, wanted: &str, formats: &[String]) -> Error {
    Error::Unsupported(format!(
        "{uri} does not accept {wanted}; it accepts {}",
        if formats.is_empty() {
            "nothing it will admit to".to_string()
        } else {
            formats.join(", ")
        }
    ))
}

fn keyword(name: IppAttributeName, value: &str) -> Result<IppAttribute> {
    let keyword = IppKeyword::new(value)
        .map_err(|e| Error::Unsupported(format!("{name} cannot be set to {value:?}: {e}")))?;
    Ok(IppAttribute::new(name, IppValue::Keyword(keyword)))
}

#[allow(unused_imports)]
use StatusCode as _StatusCodeInUse;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn a_printer_that_lists_the_format_accepts_it() {
        let formats = vec![
            "image/pwg-raster".to_string(),
            "application/pdf".to_string(),
        ];
        assert!(accepts(&formats, "application/pdf"));
        assert!(accepts(&formats, "Application/PDF"));
        assert!(!accepts(&formats, "image/jpeg"));
    }

    #[test]
    fn a_printer_that_lists_nothing_is_accepted() {
        assert!(accepts(&[], "application/pdf"));
    }

    const TEXT: u8 = 0x41;
    const KEYWORD: u8 = 0x44;

    /// Writes attributes in the on-the-wire form. An empty name makes the
    /// value an additional one for the attribute before it.
    fn push_attributes(body: &mut Vec<u8>, attributes: &[(u8, &str, &str)]) {
        for (tag, name, value) in attributes {
            body.push(*tag);
            body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(&(value.len() as u16).to_be_bytes());
            body.extend_from_slice(value.as_bytes());
        }
    }

    /// The shortest well-formed IPP response: a version, a status, a request
    /// id and the operation attributes every response must carry.
    fn ipp_response(status: u16) -> Vec<u8> {
        ipp_response_with(status, &[], &[])
    }

    /// The same, carrying extra operation attributes and a printer group.
    fn ipp_response_with(
        status: u16,
        operation: &[(u8, &str, &str)],
        printer: &[(u8, &str, &str)],
    ) -> Vec<u8> {
        let mut body = vec![0x02, 0x00];
        body.extend_from_slice(&status.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());

        body.push(0x01); // operation-attributes-tag
        push_attributes(
            &mut body,
            &[
                (0x47, "attributes-charset", "utf-8"),
                (0x48, "attributes-natural-language", "en-us"),
            ],
        );
        push_attributes(&mut body, operation);

        if !printer.is_empty() {
            body.push(0x04); // printer-attributes-tag
            push_attributes(&mut body, printer);
        }

        body.push(0x03); // end-of-attributes-tag
        body
    }

    /// Reads one HTTP request, so the client is never answered before it has
    /// finished writing and never left waiting on a half-read socket.
    fn drain_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        use std::io::Read as _;

        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return buffer,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
            let Some(head_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
            match head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
            {
                // A declared length: wait for exactly that much.
                Some(length) if buffer.len() >= head_end + 4 + length => return buffer,
                Some(_) => continue,
                // Chunked: wait for the terminating zero-length chunk.
                None if buffer.ends_with(b"0\r\n\r\n") => return buffer,
                None => continue,
            }
        }
    }

    /// A printer that answers with `responses` in turn, repeating the last
    /// one, and counts the documents it was offered.
    fn fake_printer(responses: Vec<Vec<u8>>) -> (Uri, Arc<AtomicUsize>) {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let offered = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&offered);
        std::thread::spawn(move || {
            for (nth, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let request = drain_request(&mut stream);

                if request.windows(DOCUMENT.len()).any(|w| w == DOCUMENT) {
                    counter.fetch_add(1, Ordering::SeqCst);
                }

                let body = responses.get(nth).or(responses.last()).unwrap();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });

        let uri = format!("ipp://127.0.0.1:{port}/ipp/print").parse().unwrap();
        (uri, offered)
    }

    /// A retry schedule short enough for a test to sit through.
    fn brisk(attempts: u32) -> Retry {
        Retry {
            attempts,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
        }
    }

    fn sink_for(responses: Vec<Vec<u8>>, retry: Retry) -> (IppSink, Arc<AtomicUsize>) {
        let (uri, offered) = fake_printer(responses);
        let mut sink = IppSink::new(uri, Duration::from_secs(5));
        sink.set_retry(retry);
        (sink, offered)
    }

    fn statuses(codes: &[u16]) -> Vec<Vec<u8>> {
        codes.iter().map(|code| ipp_response(*code)).collect()
    }

    fn a_job() -> crate::Job<'static> {
        crate::Job::new(DOCUMENT, "application/pdf", "test")
    }

    const DOCUMENT: &[u8] = b"%PDF-1.7";
    const BUSY: u16 = 0x0507;
    const OK: u16 = 0x0000;
    const FORMAT_NOT_SUPPORTED: u16 = 0x040A;

    #[tokio::test]
    async fn a_busy_printer_is_asked_again_until_it_takes_the_job() {
        let (sink, offered) = sink_for(statuses(&[BUSY, BUSY, OK]), brisk(4));

        let started = std::time::Instant::now();
        sink.print(&a_job())
            .await
            .expect("the third answer was a yes");

        assert_eq!(
            offered.load(Ordering::SeqCst),
            3,
            "wrong number of attempts"
        );
        // 10ms before the second attempt, 20ms before the third.
        assert!(
            started.elapsed() >= Duration::from_millis(30),
            "it did not wait between attempts: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_printer_that_stays_busy_gives_up_after_the_budget() {
        let (sink, offered) = sink_for(statuses(&[BUSY]), brisk(3));

        let error = sink.print(&a_job()).await.unwrap_err();

        assert!(matches!(error, Error::Rejected { .. }), "{error:?}");
        assert!(error.is_retryable());
        assert_eq!(
            offered.load(Ordering::SeqCst),
            3,
            "the budget was not honoured"
        );
    }

    #[tokio::test]
    async fn an_answer_that_will_not_change_is_not_retried() {
        // Sending an unsupported document a second time only wastes the wait.
        let (sink, offered) = sink_for(statuses(&[FORMAT_NOT_SUPPORTED]), brisk(4));

        let error = sink.print(&a_job()).await.unwrap_err();

        assert!(!error.is_retryable(), "{error:?}");
        assert_eq!(
            offered.load(Ordering::SeqCst),
            1,
            "it should not have retried"
        );
    }

    #[tokio::test]
    async fn retrying_can_be_turned_off_altogether() {
        let (sink, offered) = sink_for(statuses(&[BUSY, OK]), Retry::none());

        assert!(sink.print(&a_job()).await.is_err());
        assert_eq!(offered.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_rejection_carries_both_messages_the_printer_offered() {
        let (sink, _) = sink_for(
            vec![ipp_response_with(
                BUSY,
                &[
                    (TEXT, "status-message", "Queue full"),
                    (TEXT, "detailed-status-message", "5 of 5 jobs held"),
                ],
                &[],
            )],
            Retry::none(),
        );

        let error = sink.print(&a_job()).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Busy"), "{message}");
        assert!(message.contains("Queue full"), "{message}");
        assert!(message.contains("5 of 5 jobs held"), "{message}");
    }

    #[tokio::test]
    async fn a_silent_rejection_is_explained_by_the_printers_own_state() {
        // Your printer said only "Busy". Asking it what is wrong is the one
        // way to turn that into something a person can act on.
        let (sink, offered) = sink_for(
            vec![
                // The offer is refused with no explanation at all...
                ipp_response(BUSY),
                // ...so the printer is asked after its health.
                ipp_response_with(
                    OK,
                    &[],
                    &[
                        (KEYWORD, "printer-state-reasons", "media-empty"),
                        (KEYWORD, "", "toner-low"),
                    ],
                ),
            ],
            Retry::none(),
        );

        let error = sink.print(&a_job()).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("media-empty"), "{message}");
        assert!(message.contains("toner-low"), "{message}");
        // Asking after the printer is not another go at the job.
        assert_eq!(offered.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_printer_that_says_it_is_fine_adds_nothing() {
        let (sink, _) = sink_for(
            vec![
                ipp_response(FORMAT_NOT_SUPPORTED),
                ipp_response_with(OK, &[], &[(KEYWORD, "printer-state-reasons", "none")]),
            ],
            Retry::none(),
        );

        let error = sink.print(&a_job()).await.unwrap_err();
        // "none" is noise, not a reason, so the message keeps its shape.
        assert!(!error.to_string().contains("none"), "{error}");
    }

    #[test]
    fn a_timeout_is_never_retried() {
        // The printer may have taken the job before it went quiet, so a
        // second attempt risks printing the document twice.
        let error = Error::Timeout {
            uri: "ipp://printer.lan/ipp/print".into(),
            timeout: Duration::from_secs(1),
        };
        assert!(!error.is_retryable());
    }

    /// A socket that accepts a connection and then says nothing, which is
    /// how a wedged printer behaves: the handshake succeeds, so the connect
    /// timeout never fires.
    fn stalled_printer() -> Uri {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(30));
                drop(stream);
            }
        });
        format!("ipp://127.0.0.1:{port}/ipp/print").parse().unwrap()
    }

    #[tokio::test]
    async fn a_printer_that_never_answers_times_out_instead_of_hanging() {
        let sink = IppSink::new(stalled_printer(), Duration::from_millis(500));

        let started = std::time::Instant::now();
        let error = sink
            .print(&a_job())
            .await
            .expect_err("a printer that never answers must not look like a success");

        assert!(
            matches!(error, Error::Timeout { .. }),
            "expected a timeout, got {error:?}"
        );
        // The point of the exercise: it came back, and quickly.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
        // The message names the printer, so a log line is enough to act on.
        assert!(error.to_string().contains("127.0.0.1"), "{error}");
    }

    #[tokio::test]
    async fn asking_a_stalled_printer_for_its_formats_times_out_too() {
        // The startup check goes through the same client, so it must not be
        // able to wedge a process before it ever serves a job.
        let sink = IppSink::new(stalled_printer(), Duration::from_millis(500));
        let error = sink.document_formats().await.unwrap_err();
        assert!(
            matches!(error, Error::Timeout { .. }),
            "expected a timeout, got {error:?}"
        );
    }
}
