//! Printing through the local operating system's printing subsystem.
//!
//! Every backend here is synchronous and spawns at most one child process, so
//! this module is usable from a plain thread as well as from an async
//! runtime (see [`crate::Sink`], which does the latter).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// Job metadata for a local backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalJob {
    /// A title for the printer's queue.
    pub name: String,
    /// The queue to print on, or the platform's default one.
    pub printer: Option<String>,
    pub copies: u32,
    /// A PWG media name, e.g. `iso_a4_210x297mm`.
    pub media: Option<String>,
    /// `one-sided`, `two-sided-long-edge` or `two-sided-short-edge`. Only
    /// [`Local::Lp`] can pass this on; see [`Local::print_file`].
    pub sides: String,
    /// How long the print command may run before it is killed.
    pub timeout: Duration,
}

impl LocalJob {
    /// An empty `printer` is no printer at all, and means the default queue.
    pub fn new(job: &crate::Job<'_>, printer: Option<&str>, timeout: Duration) -> Self {
        LocalJob {
            name: job.name.to_string(),
            printer: printer.filter(|name| !name.is_empty()).map(str::to_string),
            copies: job.copies,
            media: job.media.map(str::to_string),
            sides: job.sides.to_string(),
            timeout,
        }
    }

    /// Whether the job asks for duplex printing.
    fn is_duplex(&self) -> bool {
        !self.sides.is_empty() && self.sides != "one-sided"
    }
}

/// A printer the operating system knows about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Printer {
    /// The queue name, to pass as [`LocalJob::printer`].
    pub name: String,
    /// Whether a job with no printer named goes here.
    pub is_default: bool,
    /// Driver name, where the subsystem reports one.
    pub driver: Option<String>,
}

/// How a document reaches the printing subsystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Local {
    /// CUPS `lp`.
    Lp,
    /// BSD `lpr`.
    Lpr,
    /// SumatraPDF binary.
    Sumatra(PathBuf),
    /// In-process Windows Print Spooler (Windows only).
    WinSpooler,
    /// Custom command. `{file}` and `{printer}` are substituted.
    Command(String),
    /// Discard the job.
    DryRun,
}

impl Local {
    /// A short description, for a status line.
    pub fn label(&self) -> String {
        match self {
            Local::Lp => "lp (CUPS)".into(),
            Local::Lpr => "lpr".into(),
            Local::Sumatra(path) => format!("SumatraPDF ({})", path.display()),
            Local::WinSpooler => "Windows print spooler".into(),
            Local::Command(template) => format!("command: {template}"),
            Local::DryRun => "dry run".into(),
        }
    }

    /// The backend to use when the caller states no preference.
    ///
    /// On Unix, it tries `lp`, then `lpr`, then fails.
    ///
    /// [`crate::Sink::detect`] calls this for the `auto` spec.
    #[cfg(unix)]
    pub fn default_backend() -> Result<Self> {
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

    /// The backend to use when the caller states no preference.
    ///
    /// On Windows, it tries SumatraPDF, then falls back to `WinSpooler`.
    ///
    /// [`crate::Sink::detect`] calls this for the `auto` spec.
    #[cfg(windows)]
    pub fn default_backend() -> Result<Self> {
        if let Some(exe) = find_sumatra() {
            return Ok(Local::Sumatra(exe));
        }
        if cfg!(feature = "windows-spooler") {
            Ok(Local::WinSpooler)
        } else {
            Err(Error::Unsupported(
                "SumatraPDF was not found and the windows-spooler feature is off: \
                 install SumatraPDF or pass a print command"
                    .into(),
            ))
        }
    }

    /// See the Unix documentation for this method.
    #[cfg(not(any(unix, windows)))]
    pub fn default_backend() -> Result<Self> {
        Err(Error::Unsupported(
            "unsupported platform: pass a print command".into(),
        ))
    }

    /// Prints a file, blocking until the command finishes or the job's
    /// `timeout` passes.
    pub fn print_file(&self, file: &Path, job: &LocalJob) -> Result<()> {
        if matches!(self, Local::WinSpooler) {
            return print_win_spooler(file, job);
        }

        let Some(command) = self.build(file, job)? else {
            return Ok(()); // dry run
        };
        run(command, job.timeout)
    }

    /// The printers this backend can send a job to.
    ///
    /// [`Local::Lp`] and [`Local::Lpr`] ask CUPS through `lpstat`.
    /// [`Local::WinSpooler`] and [`Local::Sumatra`] ask the Windows Print
    /// Spooler. Other backends do not support this.
    pub fn printers(&self) -> Result<Vec<Printer>> {
        match self {
            Local::Lp | Local::Lpr => cups_printers(),
            Local::WinSpooler | Local::Sumatra(_) => spooler_printers(),
            Local::Command(_) => Err(Error::Unsupported(
                "querying printers is not supported for this backend".into(),
            )),
            Local::DryRun => Ok(Vec::new()),
        }
    }

    /// The default printer, if known.
    pub fn default_printer(&self) -> Option<String> {
        match self {
            Local::Lp | Local::Lpr => cups_default(),
            Local::WinSpooler | Local::Sumatra(_) => spooler_default(),
            Local::Command(_) | Local::DryRun => None,
        }
    }

    /// Builds the child process. `None` means there is nothing to run.
    fn build(&self, file: &Path, job: &LocalJob) -> Result<Option<Command>> {
        let printer = job.printer.as_deref();
        let command = match self {
            Local::DryRun | Local::WinSpooler => return Ok(None),
            Local::Lp => {
                let mut c = Command::new("lp");
                if let Some(name) = printer {
                    c.arg("-d").arg(name);
                }
                if !job.name.is_empty() {
                    c.arg("-t").arg(&job.name);
                }
                if job.copies > 1 {
                    c.arg("-n").arg(job.copies.to_string());
                }
                if let Some(media) = &job.media {
                    c.arg("-o").arg(format!("media={media}"));
                }
                if job.is_duplex() {
                    c.arg("-o").arg(format!("sides={sides}", sides = job.sides));
                }
                c.arg("--").arg(file);
                c
            }
            Local::Lpr => {
                let mut c = Command::new("lpr");
                if let Some(name) = printer {
                    c.arg("-P").arg(name);
                }
                if !job.name.is_empty() {
                    c.arg("-T").arg(&job.name);
                }
                if job.copies > 1 {
                    c.arg("-#").arg(job.copies.to_string());
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
                // Sumatra takes its print options as one comma-separated
                // list, in which `<n>x` is the copy count.
                if job.copies > 1 {
                    c.arg("-print-settings")
                        .arg(format!("{copies}x", copies = job.copies));
                }
                c.arg("-silent").arg("-exit-when-done").arg(file);
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

/// Runs one print command, blocking until it exits or `timeout` passes.
fn run(mut command: Command, timeout: Duration) -> Result<()> {
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
                    exit_description(&status),
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

/// How a child process ended, for an error message.
fn exit_description(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("exit {code}"))
        .unwrap_or_else(|| "terminated".into())
}

/// Asks CUPS which destinations it has.
fn cups_printers() -> Result<Vec<Printer>> {
    let listed = lpstat(&["-e"])?;
    Ok(parse_destinations(&listed, cups_default().as_deref()))
}

/// The CUPS default destination, if one is set.
fn cups_default() -> Option<String> {
    parse_default(&lpstat(&["-d"]).ok()?)
}

/// Runs `lpstat` with `args`, returning its standard output.
fn lpstat(args: &[&str]) -> Result<String> {
    let output = Command::new("lpstat")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            Error::Unsupported(format!(
                "could not run lpstat to list printers (is CUPS installed?): {e}"
            ))
        })?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(Error::Backend(format!(
            "lpstat {args} failed ({status}){detail}",
            args = args.join(" "),
            status = exit_description(&output.status),
            detail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {}", truncate(detail, 300))
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads the destinations out of `lpstat -e` output, one name per line.
fn parse_destinations(stdout: &str, default: Option<&str>) -> Vec<Printer> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| Printer {
            is_default: default == Some(name),
            name: name.to_string(),
            // CUPS has a PPD rather than a driver name worth showing.
            driver: None,
        })
        .collect()
}

/// Reads the name out of `lpstat -d` output, which is one line reading
/// `system default destination: NAME`, or saying there is none.
fn parse_default(stdout: &str) -> Option<String> {
    let (_, name) = stdout.lines().next()?.split_once(':')?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Asks the Windows print spooler which printers it has.
#[cfg(all(windows, feature = "windows-spooler"))]
fn spooler_printers() -> Result<Vec<Printer>> {
    // Windows can leave the "default" attribute set on more than one
    // printer, so take the spooler's own answer as the single authority.
    let default = spooler_default();
    Ok(printers::get_printers()
        .into_iter()
        .map(|printer| Printer {
            is_default: default.as_deref() == Some(printer.name.as_str()),
            name: printer.name,
            driver: Some(printer.driver_name),
        })
        .collect())
}

/// See the Windows documentation for this function.
#[cfg(not(all(windows, feature = "windows-spooler")))]
fn spooler_printers() -> Result<Vec<Printer>> {
    Err(spooler_unavailable())
}

/// The printer the Windows spooler sends unaddressed jobs to.
#[cfg(all(windows, feature = "windows-spooler"))]
fn spooler_default() -> Option<String> {
    printers::get_default_printer().map(|printer| printer.name)
}

/// See the Windows documentation for this function.
#[cfg(not(all(windows, feature = "windows-spooler")))]
fn spooler_default() -> Option<String> {
    None
}

/// Why the Windows print spooler is out of reach here.
pub(crate) fn spooler_unavailable() -> Error {
    if !cfg!(windows) {
        Error::Unsupported(
            "the Windows print spooler is Windows-only; on Unix use lp or lpr".into(),
        )
    } else {
        Error::Unsupported(
            "the Windows print spooler needs the windows-spooler feature, which this build \
             turned off"
                .into(),
        )
    }
}

/// Substitutes `{file}` and `{printer}` in a command template.
pub fn expand(template: &str, file: &Path, printer: Option<&str>) -> String {
    template
        .replace("{file}", &file.display().to_string())
        .replace("{printer}", printer.unwrap_or(""))
}

#[cfg(all(windows, feature = "windows-spooler"))]
fn print_win_spooler(file: &Path, job: &LocalJob) -> Result<()> {
    use printers::common::base::job::PrinterJobOptions;

    let target = match job.printer.as_deref() {
        Some(name) => printers::get_printer_by_name(name)
            .ok_or_else(|| Error::Backend(format!("no printer named {name:?}")))?,
        None => printers::get_default_printer()
            .ok_or_else(|| Error::Backend("no default printer is set on this machine".into()))?,
    };
    let path = file
        .to_str()
        .ok_or_else(|| Error::Backend("the spool path is not valid UTF-8".into()))?;

    for _ in 0..job.copies.max(1) {
        target
            .print_file(
                path,
                PrinterJobOptions {
                    name: Some(job.name.as_str()),
                    ..PrinterJobOptions::none()
                },
            )
            .map_err(|e| Error::Backend(format!("the printer rejected the job: {}", e.message)))?;
    }
    Ok(())
}

#[cfg(not(all(windows, feature = "windows-spooler")))]
fn print_win_spooler(_file: &Path, _job: &LocalJob) -> Result<()> {
    Err(spooler_unavailable())
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

    fn job(printer: Option<&str>, copies: u32) -> LocalJob {
        LocalJob {
            name: "my job".into(),
            printer: printer.filter(|name| !name.is_empty()).map(str::to_string),
            copies,
            media: Some("A4".into()),
            sides: "one-sided".into(),
            timeout: Duration::from_secs(10),
        }
    }

    fn command_args(backend: &Local, job: &LocalJob) -> Vec<String> {
        backend
            .build(Path::new("/tmp/job.pdf"), job)
            .unwrap()
            .unwrap()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_default_backend_is_one_of_the_platform_chain() {
        match Local::default_backend() {
            Ok(backend) if cfg!(unix) => {
                assert!(matches!(backend, Local::Lp | Local::Lpr), "{backend:?}")
            }
            Ok(backend) if cfg!(windows) => assert!(
                matches!(backend, Local::Sumatra(_) | Local::WinSpooler),
                "{backend:?}"
            ),
            Ok(backend) => panic!("unexpected backend {backend:?}"),
            // A machine with nothing installed is allowed to say so.
            Err(error) => assert!(error.to_string().contains("no print"), "{error}"),
        }
    }

    #[test]
    fn lp_is_told_the_printer_the_title_the_copies_and_the_media() {
        let args = command_args(&Local::Lp, &job(Some("HP"), 3));
        assert!(args.windows(2).any(|w| w == ["-d", "HP"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["-t", "my job"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["-n", "3"]), "{args:?}");
        assert!(args.contains(&"media=A4".to_string()), "{args:?}");
        assert!(args.last().unwrap().ends_with("job.pdf"));

        // One copy is the default, so it is not spelled out.
        let args = command_args(&Local::Lp, &job(None, 1));
        assert!(!args.contains(&"-n".to_string()), "{args:?}");
        assert!(!args.contains(&"-d".to_string()), "{args:?}");
    }

    #[test]
    fn lpr_is_told_the_printer_the_title_and_the_copies() {
        let args = command_args(&Local::Lpr, &job(Some("HP"), 3));
        assert!(args.windows(2).any(|w| w == ["-P", "HP"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["-T", "my job"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["-#", "3"]), "{args:?}");
        // lpr has no portable way to ask for either of these.
        assert!(!args.contains(&"media=A4".to_string()), "{args:?}");
    }

    #[test]
    fn sumatra_is_told_the_copy_count_in_its_own_spelling() {
        let exe = PathBuf::from("sumatra.exe");

        let args = command_args(&Local::Sumatra(exe.clone()), &job(Some("HP"), 3));
        assert!(
            args.windows(2).any(|w| w == ["-print-settings", "3x"]),
            "{args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["-print-to", "HP"]),
            "{args:?}"
        );

        // One copy is the default, so it is not spelled out.
        let args = command_args(&Local::Sumatra(exe), &job(None, 1));
        assert!(!args.contains(&"-print-settings".to_string()), "{args:?}");
    }

    #[test]
    fn only_lp_is_asked_for_duplex_and_only_when_it_is_wanted() {
        let mut duplex = job(None, 1);
        duplex.sides = "two-sided-long-edge".into();

        let args = command_args(&Local::Lp, &duplex);
        assert!(
            args.contains(&"sides=two-sided-long-edge".to_string()),
            "{args:?}"
        );

        // One-sided is the default, so it is not spelled out.
        let args = command_args(&Local::Lp, &job(None, 1));
        assert!(!args.iter().any(|a| a.starts_with("sides=")), "{args:?}");

        // The rest drop it rather than refuse the job.
        let args = command_args(&Local::Lpr, &duplex);
        assert!(!args.iter().any(|a| a.starts_with("sides=")), "{args:?}");
    }

    #[test]
    fn an_empty_printer_name_means_the_default_printer() {
        let args = command_args(&Local::Lp, &job(Some(""), 1));
        assert!(!args.contains(&"-d".to_string()), "{args:?}");

        // Sumatra has a separate flag for the default queue.
        let exe = PathBuf::from("sumatra.exe");
        let args = command_args(&Local::Sumatra(exe), &job(Some(""), 1));
        assert!(args.contains(&"-print-to-default".to_string()), "{args:?}");
    }

    #[test]
    fn a_job_takes_the_printer_and_the_preferences_from_a_sink_job() {
        let mut source = crate::Job::new(b"%PDF-1.7", "application/pdf", "the title");
        source.copies = 4;
        source.media = Some("iso_a4_210x297mm");
        source.sides = "two-sided-short-edge";

        let job = LocalJob::new(&source, Some("HP"), Duration::from_secs(5));
        assert_eq!(job.name, "the title");
        assert_eq!(job.printer.as_deref(), Some("HP"));
        assert_eq!(job.copies, 4);
        assert_eq!(job.media.as_deref(), Some("iso_a4_210x297mm"));
        assert!(job.is_duplex());

        // An empty name is no name at all.
        let job = LocalJob::new(&source, Some(""), Duration::from_secs(5));
        assert_eq!(job.printer, None);

        // A plain job asks for nothing.
        let job = LocalJob::new(
            &crate::Job::new(b"x", "application/pdf", "t"),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(job.copies, 1);
        assert_eq!(job.media, None);
        assert!(!job.is_duplex());
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
                .build(Path::new("/tmp/j.pdf"), &job(None, 1))
                .unwrap()
                .is_none()
        );
        assert!(
            Local::DryRun
                .print_file(Path::new("/tmp/j.pdf"), &job(None, 1))
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
            .print_file(Path::new("/tmp/j.pdf"), &job(None, 1))
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
                .print_file(Path::new("/tmp/j.pdf"), &job(None, 1))
                .is_ok()
        );
    }

    #[test]
    fn a_hanging_command_is_killed() {
        let backend = Local::Command(script("sleep 30", "ping -n 31 127.0.0.1 > nul"));
        let mut impatient = job(None, 1);
        impatient.timeout = Duration::from_millis(200);
        let error = backend
            .print_file(Path::new("/tmp/j.pdf"), &impatient)
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn each_backend_lists_the_printers_of_the_subsystem_it_prints_through() {
        // Neither of these has a queue behind it, and they differ in whether
        // that is worth an error: a dry run has none, a command may print
        // anywhere at all and simply cannot be asked.
        assert!(Local::DryRun.printers().unwrap().is_empty());
        assert_eq!(Local::DryRun.default_printer(), None);

        let error = Local::Command("mycmd {file}".into())
            .printers()
            .unwrap_err()
            .to_string();
        assert!(error.contains("not supported"), "{error}");
        assert_eq!(
            Local::Command("mycmd {file}".into()).default_printer(),
            None
        );

        // CUPS answers for both Unix backends where it is installed, and a
        // machine without it is allowed to say so.
        for backend in [Local::Lp, Local::Lpr] {
            match backend.printers() {
                Ok(printers) => assert!(
                    printers.iter().filter(|p| p.is_default).count() <= 1,
                    "more than one default: {printers:?}"
                ),
                Err(error) => assert!(error.to_string().contains("lpstat"), "{error}"),
            }
        }
    }

    #[test]
    fn the_windows_backends_list_through_the_spooler() {
        // Sumatra prints to the spooler's queues, so it is asked the same way.
        for backend in [Local::WinSpooler, Local::Sumatra("sumatra.exe".into())] {
            let listed = backend.printers();
            if cfg!(all(windows, feature = "windows-spooler")) {
                assert!(listed.is_ok(), "{:?}", listed.err());
            } else {
                // Off this platform or out of this build, it says which.
                let error = listed.unwrap_err().to_string();
                assert!(
                    error.contains("Windows-only") || error.contains("windows-spooler"),
                    "{error}"
                );
                assert_eq!(backend.default_printer(), None);
            }
        }
    }

    #[test]
    fn lpstat_output_is_read_as_names_and_the_one_default() {
        let printers = parse_destinations("HP_LaserJet\n  Office \n\nBrother\n", Some("Office"));
        assert_eq!(
            printers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["HP_LaserJet", "Office", "Brother"]
        );
        assert_eq!(
            printers
                .iter()
                .filter(|p| p.is_default)
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Office"]
        );
        assert!(printers.iter().all(|p| p.driver.is_none()));

        // A name CUPS does not list is no default here.
        let printers = parse_destinations("HP\n", Some("Gone"));
        assert!(printers.iter().all(|p| !p.is_default), "{printers:?}");
        assert!(parse_destinations("   \n\n", None).is_empty());

        assert_eq!(
            parse_default("system default destination: HP\n").as_deref(),
            Some("HP")
        );
        // Both ways CUPS can report that there is none.
        assert_eq!(parse_default("no system default destination\n"), None);
        assert_eq!(parse_default(""), None);
        // Only the first line answers the question.
        assert_eq!(
            parse_default("system default destination: HP\nand then some nonsense\n").as_deref(),
            Some("HP")
        );
    }

    #[test]
    fn long_command_output_is_trimmed() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc...");
    }
}
