//! Send a document to the IPP printer.

use std::io::Cursor;

use ipp::attribute::{IppAttribute, IppAttributeName};
use ipp::model::{DelimiterTag, StatusCode};
use ipp::payload::IppPayload;
use ipp::prelude::{AsyncIppClient, IppOperationBuilder, Uri};
use ipp::value::{IppKeyword, IppValue};

use crate::{Error, Result};

/// A printer reachable over IPP.
pub struct IppSink {
    uri: Uri,
}

impl IppSink {
    pub fn new(uri: Uri) -> Self {
        IppSink { uri }
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// A fresh client. The `ipp` client holds a connection pool, and a job
    /// may be minutes apart from the last, so one per operation keeps a stale
    /// pooled connection from failing to print.
    fn client(&self) -> AsyncIppClient {
        AsyncIppClient::new(self.uri.clone())
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
            .map_err(|e| Error::Backend(format!("could not reach {}: {e}", self.uri)))?;

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

        let response =
            self.client().send(operation).await.map_err(|e| {
                Error::Backend(format!("could not send the job to {}: {e}", self.uri))
            })?;

        let status = response.header().status_code();
        if !status.is_success() {
            let detail = response
                .attributes()
                .first_of(DelimiterTag::OperationAttributes)
                .and_then(|group| group.get(IppAttribute::STATUS_MESSAGE))
                .map(|a| a.value().to_string())
                .unwrap_or_default();
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
}
