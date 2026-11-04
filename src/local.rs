//! Printing through the local operating system's printing subsystem.
//!
//! Every backend here is synchronous and spawns at most one child process, so
//! this module is usable from a plain thread as well as from an async
//! runtime (see [`crate::Sink`], which does the latter).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// How a document reaches the printing subsystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Local {
    /// In-process Windows Print Spooler (Windows only).
    Native,
    /// CUPS `lp`.
    Lp,
    /// BSD `lpr`.
    Lpr,
    /// SumatraPDF binary.
    Sumatra(PathBuf),
    /// Adobe Acrobat or Reader in "print to printer" mode.
    Acrobat(PathBuf),
    /// Custom command. `{file}` and `{printer}` are substituted.
    Command(String),
    /// Discard the job.
    DryRun,
}

impl Local {
    /// A short description, for a status line.
    pub fn label(&self) -> String {
        match self {
            Local::Native => "Windows spooler".into(),
            Local::Lp => "lp (CUPS)".into(),
            Local::Lpr => "lpr".into(),
            Local::Sumatra(path) => format!("SumatraPDF ({})", path.display()),
            Local::Acrobat(path) => format!("Acrobat ({})", path.display()),
            Local::Command(template) => format!("command: {template}"),
            Local::DryRun => "dry run".into(),
        }
    }

    /// Whether this backend can be pointed at a named printer.
    pub fn supports_printer_choice(&self) -> bool {
        !matches!(self, Local::DryRun)
    }

    /// Picks a backend by name, or by what is installed.
    pub fn detect(explicit: Option<&str>, command: Option<&str>) -> Result<Self> {
        if let Some(template) = command {
            return Ok(Local::Command(template.to_string()));
        }
        match explicit.map(str::to_ascii_lowercase).as_deref() {
            None | Some("auto") => Self::auto(),
            Some("native") => {
                if cfg!(windows) {
                    Ok(Local::Native)
                } else {
                    Err(Error::Unsupported(
                        "the native backend is Windows-only; on Unix use lp or lpr".into(),
                    ))
                }
            }
            Some("lp") => Ok(Local::Lp),
            Some("lpr") => Ok(Local::Lpr),
            Some("sumatra") => find_sumatra().map(Local::Sumatra).ok_or_else(|| {
                Error::Unsupported("SumatraPDF was not found; pass a print command instead".into())
            }),
            Some("acrobat") => find_acrobat().map(Local::Acrobat).ok_or_else(|| {
                Error::Unsupported("Acrobat was not found; pass a print command instead".into())
            }),
            Some("dry-run") | Some("none") => Ok(Local::DryRun),
            Some(other) => Err(Error::Unsupported(format!(
                "unknown backend {other:?}; use auto, native, lp, lpr, sumatra, acrobat or dry-run"
            ))),
        }
    }

    #[cfg(unix)]
    fn auto() -> Result<Self> {
        if which("lp").is_some() {
            return Ok(Local::Lp);
        }
        if which("lpr").is_some() {
            return Ok(Local::Lpr);
        }
        Err(Error::Unsupported(
            "no printing command found: install CUPS (lp) or pass a print command".into(),
        ))
    }

    #[cfg(windows)]
    fn auto() -> Result<Self> {
        // The spooler needs no helper application, so it is the default.
        Ok(Local::Native)
    }

    #[cfg(not(any(unix, windows)))]
    fn auto() -> Result<Self> {
        Err(Error::Unsupported(
            "unsupported platform: pass a print command".into(),
        ))
    }

    /// Prints a file, blocking until the command finishes or `timeout` passes.
    pub fn print_file(
        &self,
        file: &Path,
        printer: Option<&str>,
        copies: u32,
        media: Option<&str>,
        timeout: Duration,
    ) -> Result<()> {
        if matches!(self, Local::Native) {
            return print_native(file, printer, copies);
        }

        let Some(mut command) = self.build(file, printer, copies, media)? else {
            return Ok(()); // dry run
        };

        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| Error::Backend(format!("could not start the print command: {e}")))?;

        // A wedged print command must not stall the queue for everyone else.
        let deadline = Instant::now() + timeout;
        loop {
            match child
                .try_wait()
                .map_err(|e| Error::Backend(e.to_string()))?
            {
                Some(status) => {
                    let output = child
                        .wait_with_output()
                        .map_err(|e| Error::Backend(e.to_string()))?;
                    if status.success() {
                        return Ok(());
                    }
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let detail = [stderr.trim(), stdout.trim()]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" / ");
                    return Err(Error::Backend(format!(
                        "the print command failed ({}){}",
                        status
                            .code()
                            .map(|c| format!("exit {c}"))
                            .unwrap_or_else(|| "terminated".into()),
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", truncate(&detail, 300))
                        }
                    )));
                }
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    return Err(Error::Backend(format!(
                        "the print command timed out after {}s",
                        timeout.as_secs()
                    )));
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// Builds the child process. `None` means there is nothing to run.
    fn build(
        &self,
        file: &Path,
        printer: Option<&str>,
        copies: u32,
        media: Option<&str>,
    ) -> Result<Option<Command>> {
        let printer = printer.filter(|p| !p.is_empty());
        let command = match self {
            Local::DryRun | Local::Native => return Ok(None),
            Local::Lp => {
                let mut c = Command::new("lp");
                if let Some(name) = printer {
                    c.arg("-d").arg(name);
                }
                if copies > 1 {
                    c.arg("-n").arg(copies.to_string());
                }
                if let Some(media) = media {
                    c.arg("-o").arg(format!("media={media}"));
                }
                c.arg("--").arg(file);
                c
            }
            Local::Lpr => {
                let mut c = Command::new("lpr");
                if let Some(name) = printer {
                    c.arg("-P").arg(name);
                }
                if copies > 1 {
                    c.arg("-#").arg(copies.to_string());
                }
                c.arg(file);
                c
            }
            Local::Sumatra(exe) => {
                let mut c = Command::new(exe);
                match printer {
                    Some(name) => {
                        c.arg("-print-to").arg(name);
                    }
                    None => {
                        c.arg("-print-to-default");
                    }
                }
                c.arg("-silent").arg("-exit-when-done").arg(file);
                c
            }
            Local::Acrobat(exe) => {
                let mut c = Command::new(exe);
                match printer {
                    // /t prints to a named printer, /p to the default one.
                    Some(name) => {
                        c.arg("/t").arg(file).arg(name);
                    }
                    None => {
                        c.arg("/p").arg("/h").arg(file);
                    }
                }
                c
            }
            Local::Command(template) => {
                let line = expand(template, file, printer);
                if cfg!(windows) {
                    let mut c = Command::new("cmd");
                    c.arg("/C").arg(line);
                    c
                } else {
                    let mut c = Command::new("sh");
                    c.arg("-c").arg(line);
                    c
                }
            }
        };
        Ok(Some(command))
    }
}

/// Substitutes `{file}` and `{printer}` in a command template.
pub fn expand(template: &str, file: &Path, printer: Option<&str>) -> String {
    template
        .replace("{file}", &file.display().to_string())
        .replace("{printer}", printer.unwrap_or(""))
}

#[cfg(all(windows, feature = "windows-spooler"))]
fn print_native(file: &Path, printer: Option<&str>, copies: u32) -> Result<()> {
    use printers::common::base::job::PrinterJobOptions;

    let printer = printer.filter(|p| !p.is_empty());
    let target = match printer {
        Some(name) => printers::get_printer_by_name(name)
            .ok_or_else(|| Error::Backend(format!("no printer named {name:?}")))?,
        None => printers::get_default_printer()
            .ok_or_else(|| Error::Backend("no default printer is set on this machine".into()))?,
    };
    let path = file
        .to_str()
        .ok_or_else(|| Error::Backend("the spool path is not valid UTF-8".into()))?;

    for _ in 0..copies.max(1) {
        target
            .print_file(
                path,
                PrinterJobOptions {
                    name: Some("watermark-printer"),
                    ..PrinterJobOptions::none()
                },
            )
            .map_err(|e| Error::Backend(format!("the printer rejected the job: {}", e.message)))?;
    }
    Ok(())
}

#[cfg(not(all(windows, feature = "windows-spooler")))]
fn print_native(_file: &Path, _printer: Option<&str>, _copies: u32) -> Result<()> {
    Err(Error::Unsupported(
        "the native backend needs the windows-spooler feature, which this build turned off".into(),
    ))
}

/// Looks for an executable on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.BAT;.CMD".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for directory in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = directory.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Finds SumatraPDF next to this program, on `PATH`, or where it installs.
pub fn find_sumatra() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(directory) = exe.parent()
    {
        for name in ["SumatraPDF.exe", "SumatraPDF-portable.exe"] {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(found) = which("SumatraPDF") {
        return Some(found);
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let candidate = PathBuf::from(base)
                .join("SumatraPDF")
                .join("SumatraPDF.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Finds Acrobat Reader or Acrobat.
pub fn find_acrobat() -> Option<PathBuf> {
    for exe in ["AcroRd32", "Acrobat"] {
        if let Some(found) = which(exe) {
            return Some(found);
        }
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(variable) {
            for relative in [
                r"Adobe\Acrobat Reader DC\Reader\AcroRd32.exe",
                r"Adobe\Reader 11.0\Reader\AcroRd32.exe",
                r"Adobe\Acrobat DC\Acrobat\Acrobat.exe",
            ] {
                let candidate = PathBuf::from(&base).join(relative);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value.chars().take(max).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    fn command_args(backend: &Local, printer: Option<&str>, copies: u32) -> Vec<String> {
        backend
            .build(Path::new("/tmp/job.pdf"), printer, copies, Some("A4"))
            .unwrap()
            .unwrap()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn an_explicit_backend_wins_over_detection() {
        assert_eq!(Local::detect(Some("lp"), None).unwrap(), Local::Lp);
        assert_eq!(Local::detect(Some("LPR"), None).unwrap(), Local::Lpr);
        assert_eq!(Local::detect(Some("dry-run"), None).unwrap(), Local::DryRun);
        assert!(Local::detect(Some("nonsense"), None).is_err());
    }

    #[test]
    fn a_print_command_overrides_everything() {
        assert_eq!(
            Local::detect(Some("lp"), Some("mycmd {file}")).unwrap(),
            Local::Command("mycmd {file}".into())
        );
    }

    #[test]
    fn the_native_backend_is_windows_only() {
        let chosen = Local::detect(Some("native"), None);
        if cfg!(windows) {
            assert_eq!(chosen.unwrap(), Local::Native);
        } else {
            assert!(chosen.unwrap_err().to_string().contains("Windows-only"));
        }
    }

    #[test]
    fn lp_is_told_the_printer_the_copies_and_the_media() {
        let args = command_args(&Local::Lp, Some("HP"), 3);
        assert!(args.windows(2).any(|w| w == ["-d", "HP"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["-n", "3"]), "{args:?}");
        assert!(args.contains(&"media=A4".to_string()), "{args:?}");
        assert!(args.last().unwrap().ends_with("job.pdf"));

        // One copy is the default, so it is not spelled out.
        let args = command_args(&Local::Lp, None, 1);
        assert!(!args.contains(&"-n".to_string()), "{args:?}");
        assert!(!args.contains(&"-d".to_string()), "{args:?}");
    }

    #[test]
    fn an_empty_printer_name_means_the_default_printer() {
        let args = command_args(&Local::Lp, Some(""), 1);
        assert!(!args.contains(&"-d".to_string()), "{args:?}");
    }

    #[test]
    fn templates_expand_the_file_and_the_printer() {
        let file = Path::new("/tmp/job.pdf");
        assert_eq!(
            expand("lp -d {printer} {file}", file, Some("HP")),
            "lp -d HP /tmp/job.pdf"
        );
        assert_eq!(
            expand("print {printer}{file}", file, None),
            "print /tmp/job.pdf"
        );
    }

    #[test]
    fn a_dry_run_builds_no_command_and_prints_nothing() {
        assert!(
            Local::DryRun
                .build(Path::new("/tmp/j.pdf"), None, 1, None)
                .unwrap()
                .is_none()
        );
        assert!(
            Local::DryRun
                .print_file(
                    Path::new("/tmp/j.pdf"),
                    None,
                    1,
                    None,
                    Duration::from_secs(1)
                )
                .is_ok()
        );
    }

    #[test]
    fn a_failing_command_reports_its_own_output() {
        let backend = Local::Command(script(
            "echo some-failure >&2; exit 3",
            "echo some-failure 1>&2 && exit 3",
        ));
        let error = backend
            .print_file(
                Path::new("/tmp/j.pdf"),
                None,
                1,
                None,
                Duration::from_secs(10),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("exit 3"), "{error}");
        assert!(error.contains("some-failure"), "{error}");
    }

    #[test]
    fn a_successful_command_succeeds() {
        let backend = Local::Command(script("true", "exit 0"));
        assert!(
            backend
                .print_file(
                    Path::new("/tmp/j.pdf"),
                    None,
                    1,
                    None,
                    Duration::from_secs(10)
                )
                .is_ok()
        );
    }

    #[test]
    fn a_hanging_command_is_killed() {
        let backend = Local::Command(script("sleep 30", "ping -n 31 127.0.0.1 > nul"));
        let error = backend
            .print_file(
                Path::new("/tmp/j.pdf"),
                None,
                1,
                None,
                Duration::from_millis(200),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn long_command_output_is_trimmed() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc...");
    }
}
