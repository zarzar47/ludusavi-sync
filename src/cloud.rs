use std::collections::VecDeque;
use std::io::{BufRead, BufReader};

use crate::{
    lang::TRANSLATOR,
    prelude::{CommandError, CommandOutput, Error, Finality, Privacy, StrictPath, SyncDirection, run_command},
    resource::config::{App, Config},
    scan::ScanChange,
};

/// How many of rclone's own error/notice lines are kept for the failure message.
/// rclone's final "Failed to copy with N errors: last error was: ..." line is the one
/// that explains a transfer; a handful above it is enough context, and a cap keeps a
/// pathological run from turning the error into a log dump.
const MAX_RCLONE_DIAGNOSTICS: usize = 5;

/// Escape rclone filter glob metacharacters so a folder name is matched
/// literally. Game names can legally contain characters that rclone's filter
/// syntax treats specially (`[PROTOTYPE]`, `Foo {Bar}`, etc.); without escaping,
/// the include pattern silently matches nothing and the transfer copies no
/// files. See https://rclone.org/filtering/
fn escape_rclone_glob(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether a message is rclone complaining about the remote being out of storage.
///
/// Google Drive rejects every upload once the account's quota is spent
/// (`googleapi: Error 403: ... storageQuotaExceeded`); other providers word it
/// differently but all say "quota" in some form. Such a failure is not worth
/// retrying: every retry fails the same way, which is what used to turn a full
/// Drive into an eleven-minute hang before a meaningless "fatal error".
pub fn is_quota_exceeded(msg: &str) -> bool {
    if msg.contains("storageQuotaExceeded") {
        return true;
    }
    let lower = msg.to_lowercase();
    lower.contains("quota") && lower.contains("exceeded")
}

/// One classified line of rclone's `--use-json-log` output.
#[derive(Debug)]
enum ParsedRcloneLine {
    Skip { skipped: String, object: String },
    Change { msg: String, object: String },
    Progress { current: f32, max: f32, last_error: Option<String> },
    Note { level: String, msg: String },
    Ignored,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
enum RawRcloneLine {
    Skip { skipped: String, object: String },
    Change { msg: String, object: String },
    Stats { stats: RawStats },
    Note { msg: String, level: String },
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStats {
    bytes: f32,
    total_bytes: f32,
    /// The last backend error rclone saw, e.g. a Drive quota rejection. It shows up
    /// here (within a `--stats` interval of the first failure) long before rclone's
    /// own retry loop gives up - which is what makes failing fast possible.
    last_error: Option<String>,
}

/// Classify one line of rclone's JSON log. Extracted from [`RcloneProcess::events`]
/// so the shape of rclone's output can be tested without spawning a transfer.
fn parse_rclone_line(line: &str) -> ParsedRcloneLine {
    match serde_json::from_str::<RawRcloneLine>(line) {
        Ok(RawRcloneLine::Skip { skipped, object }) => ParsedRcloneLine::Skip { skipped, object },
        Ok(RawRcloneLine::Change { msg, object }) => ParsedRcloneLine::Change { msg, object },
        Ok(RawRcloneLine::Stats {
            stats:
                RawStats {
                    bytes,
                    total_bytes,
                    last_error,
                },
        }) => ParsedRcloneLine::Progress {
            current: bytes,
            max: total_bytes,
            last_error,
        },
        Ok(RawRcloneLine::Note { msg, level }) => ParsedRcloneLine::Note { level, msg },
        Err(_) => ParsedRcloneLine::Ignored,
    }
}

/// Log levels at which rclone is stating a problem (as opposed to narrating progress).
fn is_diagnostic_level(level: &str) -> bool {
    matches!(level, "error" | "notice" | "critical" | "fatal")
}

pub fn validate_cloud_config(config: &Config, cloud_path: &str) -> Result<Remote, Error> {
    if !config.apps.rclone.is_valid() {
        return Err(Error::RcloneUnavailable);
    }
    let Some(remote) = config.cloud.remote.clone() else {
        return Err(Error::CloudNotConfigured);
    };
    validate_cloud_path(cloud_path)?;
    Ok(remote)
}

pub fn validate_cloud_path(path: &str) -> Result<(), Error> {
    if path.is_empty() || path == "/" {
        Err(Error::CloudPathInvalid)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CloudChange {
    pub path: String,
    pub change: ScanChange,
}

#[derive(Clone, Debug)]
pub enum RcloneProcessEvent {
    Progress { current: f32, max: f32 },
    Change(CloudChange),
    /// A problem rclone stated in its log (error/notice level). Only quota errors are
    /// acted on by callers so far; the rest are kept by [`RcloneProcess`] itself for
    /// the failure message.
    Notice { level: String, msg: String },
}

#[derive(Debug)]
pub struct RcloneProcess {
    program: String,
    args: Vec<String>,
    child: std::process::Child,
    stderr: Option<BufReader<std::process::ChildStderr>>,
    /// The tail of what rclone actually complained about, kept as it streams past
    /// [`Self::events`]. The JSON-log runs drain their own stderr for progress, so by
    /// the time [`Self::succeeded`] reads the streams there's nothing left to report -
    /// without this, a failed transfer surfaces as a bare exit code with no reason.
    diagnostics: VecDeque<String>,
}

impl RcloneProcess {
    pub fn launch(program: String, args: Vec<String>) -> Result<Self, CommandError> {
        let mut command = std::process::Command::new(&program);
        command
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
        }

        log::debug!("Running command: {} {:?}", &program, &args);

        let mut child = command.spawn().map_err(|e| {
            let e = CommandError::Launched {
                program: program.clone(),
                args: args.clone(),
                raw: e.to_string(),
            };
            log::error!("Rclone failed: {e:?}");
            e
        })?;

        let stderr = child.stderr.take().map(BufReader::new);
        Ok(Self {
            program,
            args,
            child,
            stderr,
            diagnostics: VecDeque::new(),
        })
    }

    /// Remember one of rclone's problem lines for the eventual failure message,
    /// collapsing a repeated line (rclone retries the same error several times) and
    /// keeping only the tail.
    fn push_diagnostic(&mut self, level: &str, msg: &str) {
        if self.diagnostics.back().is_some_and(|last| last.ends_with(msg)) {
            return;
        }
        self.diagnostics.push_back(format!("[{level}] {msg}"));
        while self.diagnostics.len() > MAX_RCLONE_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
    }

    pub fn events(&mut self) -> Vec<RcloneProcessEvent> {
        let mut events = vec![];
        let mut notes: Vec<(String, String)> = vec![];
        let mut stats_last_error: Option<String> = None;

        if let Some(stderr) = self.stderr.as_mut() {
            for line in stderr.lines().take(10).filter_map(|x| x.ok()) {
                match parse_rclone_line(&line) {
                    ParsedRcloneLine::Skip { skipped, object } => match skipped.as_str() {
                        "copy" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Different,
                        })),
                        "delete" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Removed,
                        })),
                        raw => {
                            log::trace!("Unhandled Rclone 'skipped': {raw}");
                        }
                    },
                    ParsedRcloneLine::Change { msg, object } => match msg.as_str() {
                        "Copied (new)" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::New,
                        })),
                        "Copied (replaced existing)" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Different,
                        })),
                        "Deleted" => events.push(RcloneProcessEvent::Change(CloudChange {
                            path: object,
                            change: ScanChange::Removed,
                        })),
                        raw => {
                            log::trace!("Unhandled Rclone 'msg': {raw}");
                        }
                    },
                    ParsedRcloneLine::Progress {
                        current,
                        max,
                        last_error,
                    } => {
                        if max > 0.0 {
                            events.push(RcloneProcessEvent::Progress { current, max });
                        }
                        if last_error.is_some() {
                            stats_last_error = last_error;
                        }
                    }
                    ParsedRcloneLine::Note { level, msg } => {
                        if is_diagnostic_level(&level) {
                            notes.push((level, msg));
                        }
                    }
                    ParsedRcloneLine::Ignored => {
                        log::trace!("Unhandled Rclone message: {line}");
                    }
                }
            }
        }

        // Outside the stderr borrow: these need `&mut self`.
        for (level, msg) in notes {
            if is_quota_exceeded(&msg) {
                events.push(RcloneProcessEvent::Notice {
                    level: level.clone(),
                    msg: msg.clone(),
                });
            }
            self.push_diagnostic(&level, &msg);
        }
        // The stats stream reports the first backend error within a `--stats` interval -
        // long before rclone's own "Attempt N/N failed" log lines, whose internal
        // backoff is what used to stretch a full quota into an ~eleven-minute hang.
        if let Some(err) = stats_last_error
            && is_quota_exceeded(&err)
        {
            events.push(RcloneProcessEvent::Notice {
                level: "error".to_string(),
                msg: err,
            });
        }

        if !events.is_empty() {
            log::trace!("New Rclone events: {events:?}");
        }
        events
    }

    pub fn succeeded(&mut self) -> Option<Result<(), CommandError>> {
        let res = match self.child.try_wait() {
            Ok(Some(status)) => match status.code() {
                Some(0) => Some(Ok(())),
                Some(code) => {
                    let stdout = self.child.stdout.as_mut().and_then(|x| {
                        let lines = BufReader::new(x).lines().map_while(Result::ok).collect::<Vec<_>>();
                        (!lines.is_empty()).then_some(lines.join("\n"))
                    });
                    let stderr = self.stderr.as_mut().and_then(|x| {
                        let lines = x.lines().map_while(Result::ok).collect::<Vec<_>>();
                        (!lines.is_empty()).then_some(lines.join("\n"))
                    });
                    // `events()` already drained the JSON log; the classified tail is
                    // what actually explains the failure (and is human-readable, unlike
                    // the raw JSON that would otherwise leak into the message).
                    let stderr = if self.diagnostics.is_empty() {
                        stderr
                    } else {
                        Some(self.diagnostics.iter().cloned().collect::<Vec<_>>().join("\n"))
                    };

                    Some(Err(CommandError::Exited {
                        program: self.program.clone(),
                        args: self.args.clone(),
                        code,
                        stdout,
                        stderr,
                    }))
                }
                None => Some(Err(CommandError::Terminated {
                    program: self.program.clone(),
                    args: self.args.clone(),
                })),
            },
            Ok(None) => None,
            Err(_) => Some(Err(CommandError::Terminated {
                program: self.program.clone(),
                args: self.args.clone(),
            })),
        };

        if let Some(Ok(_)) = &res {
            log::debug!("Rclone succeeded");
        }
        if let Some(Err(e)) = &res {
            log::error!("Rclone failed: {e:?}");
        }

        res
    }

    pub fn kill(&mut self) -> Result<(), std::io::Error> {
        let res = self.child.kill();
        if let Err(e) = &res {
            log::error!("Unable to kill child process for Rclone: {e:?}");
        }
        res
    }

    /// Block until the child has exited, releasing it as a zombie. Used after [`Self::kill`].
    pub fn reap(&mut self) -> Result<(), std::io::Error> {
        self.child.wait().map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteChoice {
    None,
    Custom,
    Box,
    Dropbox,
    Ftp,
    GoogleDrive,
    OneDrive,
    Smb,
    WebDav,
}

impl RemoteChoice {
    pub const ALL: &'static [Self] = &[
        Self::None,
        Self::Box,
        Self::Dropbox,
        Self::GoogleDrive,
        Self::OneDrive,
        Self::Ftp,
        Self::Smb,
        Self::WebDav,
        Self::Custom,
    ];
}

impl ToString for RemoteChoice {
    fn to_string(&self) -> String {
        match self {
            Self::None => TRANSLATOR.none_label(),
            Self::Custom => TRANSLATOR.custom_label(),
            Self::Box => "Box".to_string(),
            Self::Dropbox => "Dropbox".to_string(),
            Self::Ftp => "FTP".to_string(),
            Self::GoogleDrive => "Google Drive".to_string(),
            Self::OneDrive => "OneDrive".to_string(),
            Self::Smb => "SMB".to_string(),
            Self::WebDav => "WebDAV".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename = "camelCase")] // Legacy: Should have been `rename_all`
pub enum Remote {
    Custom {
        id: String,
    },
    Box {
        id: String,
    },
    Dropbox {
        id: String,
    },
    GoogleDrive {
        id: String,
    },
    OneDrive {
        id: String,
    },
    Ftp {
        id: String,
        host: String,
        port: i32,
        username: String,
        #[serde(skip, default)]
        password: String,
    },
    Smb {
        id: String,
        host: String,
        port: i32,
        username: String,
        #[serde(skip, default)]
        password: String,
    },
    WebDav {
        id: String,
        url: String,
        username: String,
        #[serde(skip, default)]
        password: String,
        provider: WebDavProvider,
    },
}

impl Remote {
    pub fn id(&self) -> &str {
        match self {
            Remote::Box { id } => id,
            Remote::Custom { id } => id,
            Remote::Dropbox { id } => id,
            Remote::GoogleDrive { id } => id,
            Remote::OneDrive { id } => id,
            Remote::Ftp { id, .. } => id,
            Remote::Smb { id, .. } => id,
            Remote::WebDav { id, .. } => id,
        }
    }

    pub fn slug(&self) -> &str {
        match self {
            Self::Custom { .. } => "",
            Self::Box { .. } => "box",
            Self::Dropbox { .. } => "dropbox",
            Self::Ftp { .. } => "ftp",
            Self::GoogleDrive { .. } => "drive",
            Self::OneDrive { .. } => "onedrive",
            Self::Smb { .. } => "smb",
            Self::WebDav { .. } => "webdav",
        }
    }

    pub fn config_args(&self) -> Option<Vec<String>> {
        match self {
            Self::Custom { .. } => None,
            Self::Box { .. } => None,
            Self::Dropbox { .. } => None,
            Self::GoogleDrive { .. } => Some(vec!["scope=drive".to_string()]),
            Self::Ftp {
                id: _,
                host,
                port,
                username,
                password,
            } => Some(vec![
                format!("host={host}"),
                format!("port={port}"),
                format!("user={username}"),
                format!("pass={password}"),
            ]),
            Self::OneDrive { .. } => Some(vec![
                "drive_type=personal".to_string(),
                "access_scopes=Files.ReadWrite,offline_access".to_string(),
            ]),
            Self::Smb {
                id: _,
                host,
                port,
                username,
                password,
                ..
            } => Some(vec![
                format!("host={host}"),
                format!("port={port}"),
                format!("user={username}"),
                format!("pass={password}"),
            ]),
            Self::WebDav {
                id: _,
                url,
                username,
                password,
                provider,
            } => Some(vec![
                format!("url={url}"),
                format!("user={username}"),
                format!("pass={password}"),
                format!("vendor={}", provider.slug()),
            ]),
        }
    }

    pub fn needs_configuration(&self) -> bool {
        match self {
            Self::Custom { .. } => false,
            Self::Box { .. }
            | Self::Dropbox { .. }
            | Self::Ftp { .. }
            | Self::GoogleDrive { .. }
            | Self::OneDrive { .. }
            | Self::Smb { .. }
            | Self::WebDav { .. } => true,
        }
    }

    pub fn description(&self) -> Option<String> {
        match self {
            Remote::Ftp {
                host, port, username, ..
            } => Some(format!("{username}@{host}:{port}")),
            Remote::Smb {
                host, port, username, ..
            } => Some(format!("{username}@{host}:{port}")),
            Remote::WebDav { url, provider, .. } => Some(format!("{} - {}", provider.to_string(), url)),
            _ => None,
        }
    }

    pub fn generate_id() -> String {
        format!("ludusavi-{}", chrono::Utc::now().timestamp())
    }
}

impl From<Option<&Remote>> for RemoteChoice {
    fn from(value: Option<&Remote>) -> Self {
        if let Some(value) = value {
            match value {
                Remote::Custom { .. } => RemoteChoice::Custom,
                Remote::Box { .. } => RemoteChoice::Box,
                Remote::Dropbox { .. } => RemoteChoice::Dropbox,
                Remote::Ftp { .. } => RemoteChoice::Ftp,
                Remote::GoogleDrive { .. } => RemoteChoice::GoogleDrive,
                Remote::OneDrive { .. } => RemoteChoice::OneDrive,
                Remote::Smb { .. } => RemoteChoice::Smb,
                Remote::WebDav { .. } => RemoteChoice::WebDav,
            }
        } else {
            RemoteChoice::None
        }
    }
}

impl TryFrom<RemoteChoice> for Remote {
    type Error = ();

    fn try_from(value: RemoteChoice) -> Result<Self, Self::Error> {
        match value {
            RemoteChoice::None => Err(()),
            RemoteChoice::Custom => Ok(Remote::Custom {
                id: "ludusavi".to_string(),
            }),
            RemoteChoice::Box => Ok(Remote::Box {
                id: Remote::generate_id(),
            }),
            RemoteChoice::Dropbox => Ok(Remote::Dropbox {
                id: Remote::generate_id(),
            }),
            RemoteChoice::Ftp => Ok(Remote::Ftp {
                id: Remote::generate_id(),
                host: String::new(),
                port: 21,
                username: String::new(),
                password: String::new(),
            }),
            RemoteChoice::GoogleDrive => Ok(Remote::GoogleDrive {
                id: Remote::generate_id(),
            }),
            RemoteChoice::OneDrive => Ok(Remote::OneDrive {
                id: Remote::generate_id(),
            }),
            RemoteChoice::Smb => Ok(Remote::Smb {
                id: Remote::generate_id(),
                host: String::new(),
                port: 445,
                username: String::new(),
                password: String::new(),
            }),
            RemoteChoice::WebDav => Ok(Remote::WebDav {
                id: Remote::generate_id(),
                url: String::new(),
                username: String::new(),
                password: String::new(),
                provider: WebDavProvider::Other,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub enum WebDavProvider {
    #[default]
    Other,
    Nextcloud,
    Owncloud,
    Sharepoint,
    SharepointNtlm,
}

impl WebDavProvider {
    pub const ALL: &'static [Self] = &[
        Self::Other,
        Self::Nextcloud,
        Self::Owncloud,
        Self::Sharepoint,
        Self::SharepointNtlm,
    ];

    pub const ALL_CLI: &'static [&'static str] = &[
        Self::OTHER,
        Self::NEXTCLOUD,
        Self::OWNCLOUD,
        Self::SHAREPOINT,
        Self::SHAREPOINT_NTLM,
    ];
    pub const OTHER: &'static str = "other";
    const NEXTCLOUD: &'static str = "nextcloud";
    const OWNCLOUD: &'static str = "owncloud";
    const SHAREPOINT: &'static str = "sharepoint";
    const SHAREPOINT_NTLM: &'static str = "sharepoint-ntlm";
}

impl WebDavProvider {
    pub fn slug(&self) -> &str {
        match self {
            WebDavProvider::Other => Self::OTHER,
            WebDavProvider::Nextcloud => Self::NEXTCLOUD,
            WebDavProvider::Owncloud => Self::OWNCLOUD,
            WebDavProvider::Sharepoint => Self::SHAREPOINT,
            WebDavProvider::SharepointNtlm => Self::SHAREPOINT_NTLM,
        }
    }
}

impl ToString for WebDavProvider {
    fn to_string(&self) -> String {
        match self {
            Self::Other => crate::resource::manifest::Store::Other.to_string(),
            Self::Nextcloud => "Nextcloud".to_string(),
            Self::Owncloud => "Owncloud".to_string(),
            Self::Sharepoint => "Sharepoint".to_string(),
            Self::SharepointNtlm => "Sharepoint (NTLM)".to_string(),
        }
    }
}

impl std::str::FromStr for WebDavProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            Self::OTHER => Ok(Self::Other),
            Self::NEXTCLOUD => Ok(Self::Nextcloud),
            Self::OWNCLOUD => Ok(Self::Owncloud),
            Self::SHAREPOINT => Ok(Self::Sharepoint),
            Self::SHAREPOINT_NTLM => Ok(Self::SharepointNtlm),
            _ => Err(format!("invalid provider: {s}")),
        }
    }
}

pub struct Rclone {
    app: App,
    remote: Remote,
}

impl Rclone {
    pub fn new(app: App, remote: Remote) -> Self {
        Self { app, remote }
    }

    fn path(&self, path: &str) -> String {
        // Rclone doesn't allow backslashes in non-local paths:
        // https://rclone.org/docs/#syntax-of-remote-paths
        format!("{}:{}", self.remote.id(), path.replace('\\', "/"))
    }

    fn args(&self, args: &[String]) -> Vec<String> {
        let mut collected = vec![];
        if !self.app.arguments.is_empty()
            && let Some(parts) = shlex::split(&self.app.arguments)
        {
            collected.extend(parts);
        }
        for arg in args {
            collected.push(arg.to_string());
        }
        collected
    }

    fn run(&self, args: &[String], success: &[i32], privacy: Privacy) -> Result<CommandOutput, CommandError> {
        let args = self.args(args);
        let args: Vec<_> = args.iter().map(|x| x.as_str()).collect();
        run_command(self.app.path.raw(), &args, success, privacy)
    }

    /// Like `run`, but reads stdout/stderr line-by-line as the child produces them and
    /// calls `on_url_line` for every line that contains an `http://`/`https://` URL,
    /// instead of only returning once the process has fully exited. See
    /// `configure_remote_reporting_url` for why `configure_remote` needs this and
    /// nothing else currently does.
    fn run_streaming(
        &self,
        args: &[String],
        success: &[i32],
        privacy: Privacy,
        mut on_url_line: impl FnMut(&str),
    ) -> Result<CommandOutput, CommandError> {
        let args = self.args(args);
        let executable = self.app.path.raw().to_string();

        let collect_args = || {
            if privacy.sensitive() {
                vec!["**REDACTED**".to_string()]
            } else {
                args.clone()
            }
        };

        let mut command = std::process::Command::new(&executable);
        command
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
        }

        log::debug!("Running command: {} {:?}", executable, collect_args());

        let mut child = command.spawn().map_err(|e| CommandError::Launched {
            program: executable.clone(),
            args: collect_args(),
            raw: e.to_string(),
        })?;

        let stdout = child.stdout.take().expect("child stdout was piped");
        let stderr = child.stderr.take().expect("child stderr was piped");

        // Both streams are drained on their own threads and funneled through a single
        // channel back to this thread - reading only one stream synchronously risks a
        // deadlock if the *other*, unread one fills its OS pipe buffer and blocks the
        // child's write. The channel also means `on_url_line` only ever runs here, not
        // from the reader threads, so it doesn't need to be `Send`.
        let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>();
        let tx_stderr = tx.clone();
        let stdout_thread = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send((false, line)).is_err() {
                    break;
                }
            }
        });
        let stderr_thread = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx_stderr.send((true, line)).is_err() {
                    break;
                }
            }
        });

        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        for (is_stderr, line) in rx {
            // rclone's OAuth setup also logs an earlier, unrelated line containing a
            // bare `http://127.0.0.1:PORT/` ("Make sure your Redirect URL is set to
            // ..."), so matching on any http(s) URL fires on that one first. The actual
            // link to open always carries a `state=` CSRF token in the query string;
            // that notice line never does, so require it to tell them apart.
            if let Some(start) = line.find("http://").or_else(|| line.find("https://"))
                && line[start..].contains("state=")
            {
                on_url_line(line[start..].trim());
            }
            let buf = if is_stderr { &mut stderr_buf } else { &mut stdout_buf };
            buf.push_str(&line);
            buf.push('\n');
        }
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();

        let status = child.wait().map_err(|_| CommandError::Terminated {
            program: executable.clone(),
            args: collect_args(),
        })?;

        let stdout_buf = stdout_buf.trim().to_string();
        let stderr_buf = stderr_buf.trim().to_string();
        let code = status.code().unwrap_or(1);

        if success.contains(&code) {
            log::debug!("Command succeeded with {code}: {executable}");
            Ok(CommandOutput {
                code,
                stdout: stdout_buf,
                stderr: stderr_buf,
            })
        } else {
            log::error!("Command failed with {code}: {executable}");
            log::error!("Command stdout: {stdout_buf}");
            log::error!("Command stderr: {stderr_buf}");
            Err(CommandError::Exited {
                program: executable,
                args: collect_args(),
                code,
                stdout: (!stdout_buf.is_empty()).then_some(stdout_buf),
                stderr: (!stderr_buf.is_empty()).then_some(stderr_buf),
            })
        }
    }

    fn obscure(&self, credential: &str) -> Result<String, CommandError> {
        let out = self.run(&["obscure".to_string(), credential.to_string()], &[0], Privacy::Private)?;
        Ok(out.stdout)
    }

    pub fn configure_remote(&self) -> Result<(), CommandError> {
        self.configure_remote_reporting_url(|_| {})
    }

    /// Same as `configure_remote`, but calls `on_auth_url` as soon as rclone prints
    /// the OAuth browser link (Box/Dropbox/Google Drive/OneDrive all drive rclone's
    /// own OAuth flow here), rather than only after the whole command finishes.
    ///
    /// This exists because `run`/`run_command` capture output via `Command::output()`,
    /// which blocks until the child process *fully exits* - for these remotes, that
    /// means only after the user has already finished (or given up on) the browser
    /// approval, which can take minutes. If rclone can't auto-open a browser itself
    /// (e.g. no working default-browser association, as seen live on this project's
    /// own dev Steam Deck), the link it prints as a fallback was never reaching the
    /// caller until it was already too late to be useful - the process just seemed to
    /// hang with zero feedback. Streaming stdout/stderr line-by-line via
    /// `run_streaming` instead lets the caller show that link immediately, so the user
    /// can always copy/paste it manually regardless of whether auto-open works.
    pub fn configure_remote_reporting_url(&self, on_auth_url: impl FnMut(&str)) -> Result<(), CommandError> {
        if !self.remote.needs_configuration() {
            return Ok(());
        }

        let mut privacy = Privacy::Public;

        let mut remote = self.remote.clone();
        match &mut remote {
            Remote::Custom { .. }
            | Remote::Box { .. }
            | Remote::Dropbox { .. }
            | Remote::GoogleDrive { .. }
            | Remote::OneDrive { .. } => {}
            Remote::Ftp { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
            Remote::Smb { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
            Remote::WebDav { password, .. } => {
                privacy = Privacy::Private;
                *password = self.obscure(password)?;
            }
        }

        let mut args = vec![
            "config".to_string(),
            "create".to_string(),
            remote.id().to_string(),
            remote.slug().to_string(),
        ];

        if let Some(config_args) = remote.config_args() {
            args.extend(config_args);
        }

        self.run_streaming(&args, &[0], privacy, on_auth_url)?;
        Ok(())
    }

    pub fn unconfigure_remote(&self) -> Result<(), CommandError> {
        if !self.remote.needs_configuration() {
            return Ok(());
        }

        let args = vec!["config".to_string(), "delete".to_string(), self.remote.id().to_string()];

        self.run(&args, &[0], Privacy::Public)?;
        Ok(())
    }

    pub fn sync(
        &self,
        local: &StrictPath,
        remote_path: &str,
        direction: SyncDirection,
        finality: Finality,
        game_dirs: &[String],
    ) -> Result<RcloneProcess, CommandError> {
        if direction == SyncDirection::Upload && !local.exists() {
            // Rclone will fail with exit code 3 if the local folder does not exist.
            _ = local.create_dirs();
        }

        let mut args = vec![
            "sync".to_string(),
            "-v".to_string(),
            "--use-json-log".to_string(),
            "--stats=100ms".to_string(),
        ];

        if finality.preview() {
            args.push("--dry-run".to_string());
        }

        for game_dir in game_dirs {
            // Inclusion rules are file-based, so we have to add `**`.
            args.push(format!("--include=/{game_dir}/**"));
        }

        match direction {
            SyncDirection::Upload => {
                args.push(local.render());
                args.push(self.path(remote_path));
            }
            SyncDirection::Download => {
                args.push(self.path(remote_path));
                args.push(local.render());
            }
        }

        RcloneProcess::launch(self.app.path.raw().into(), self.args(&args))
    }

    /// Additive copy: copies files from source to destination without deleting
    /// anything on the destination. Unlike `sync`, this will NOT remove files
    /// on the remote that aren't in the source. This is the key difference
    /// that makes per-game cloud sync safe - pushing game A won't delete game B.
    pub fn copy(
        &self,
        local: &StrictPath,
        remote_path: &str,
        direction: SyncDirection,
        finality: Finality,
        game_dirs: &[String],
    ) -> Result<RcloneProcess, CommandError> {
        if direction == SyncDirection::Upload && !local.exists() {
            _ = local.create_dirs();
        }

        let mut args = vec![
            "copy".to_string(),
            "-v".to_string(),
            "--use-json-log".to_string(),
            "--stats=100ms".to_string(),
        ];

        if finality.preview() {
            args.push("--dry-run".to_string());
        }

        for game_dir in game_dirs {
            let escaped = escape_rclone_glob(game_dir);
            args.push(format!("--include=/{escaped}/**"));
        }
        // Always carry the per-game sync metadata alongside the transfer so a
        // push publishes it and a pull retrieves it. This is a file at the
        // backup root, so it must be matched as a file (no trailing `/**`).
        args.push(format!(
            "--include=/{}",
            crate::resource::sync_state::SyncStateFile::FILE_NAME
        ));

        match direction {
            SyncDirection::Upload => {
                args.push(local.render());
                args.push(self.path(remote_path));
            }
            SyncDirection::Download => {
                args.push(self.path(remote_path));
                args.push(local.render());
            }
        }

        RcloneProcess::launch(self.app.path.raw().into(), self.args(&args))
    }
}

#[cfg(test)]
mod tests {
    use super::{ParsedRcloneLine, escape_rclone_glob, is_quota_exceeded, parse_rclone_line};

    #[test]
    fn escapes_glob_metacharacters_in_folder_names() {
        assert_eq!(escape_rclone_glob("Portal 2"), "Portal 2");
        assert_eq!(escape_rclone_glob("[PROTOTYPE]"), "\\[PROTOTYPE\\]");
        assert_eq!(escape_rclone_glob("Foo {Bar}"), "Foo \\{Bar\\}");
        assert_eq!(escape_rclone_glob("What?"), "What\\?");
        assert_eq!(escape_rclone_glob("A*B"), "A\\*B");
    }

    // The fixtures below are real lines from a failed Google Drive transfer
    // (a full-quota `rclone copy` run captured2026-10-12), trimmed of nothing.

    #[test]
    fn quota_rejections_are_recognized_in_every_wording_rclone_uses() {
        assert!(is_quota_exceeded(
            "googleapi: Error403: The user's Drive storage quota has been exceeded., storageQuotaExceeded"
        ));
        assert!(is_quota_exceeded("storageQuotaExceeded"));
        assert!(is_quota_exceeded("The user's Drive storage quota has been exceeded."));
        assert!(!is_quota_exceeded("connection reset by peer"));
        assert!(!is_quota_exceeded("rate limit exceeded"));
    }

    #[test]
    fn the_final_failure_notice_parses_as_a_diagnostic_note() {
        // This is the line that explains *why* a transfer died - the one that used to
        // be drained by `events()` and thrown away, leaving a bare exit code.
        let line = r#"{"time":"2026-10-12T01:15:40.650157875+05:00","level":"notice","msg":"Failed to copy with27 errors: last error was: googleapi: Error403: The user's Drive storage quota has been exceeded., storageQuotaExceeded","source":"cmd/cmd.go:336"}"#;
        match parse_rclone_line(line) {
            ParsedRcloneLine::Note { level, msg } => {
                assert_eq!(level, "notice");
                assert!(is_quota_exceeded(&msg), "the note must carry the quota reason");
            }
            other => panic!("expected a note, got {other:?}"),
        }
    }

    #[test]
    fn stats_lines_carry_progress_and_the_first_backend_error() {
        let line = r#"{"time":"2026-10-12T01:14:56.606935564+05:00","level":"info","msg":"\nTransferred:   \t    2.289 MiB / 2.289 MiB, 100%\n","stats":{"bytes":2399924.0,"checks":6,"elapsedTime":630.0,"errors":27,"fatalError":false,"lastError":"googleapi: Error403: The user's Drive storage quota has been exceeded., storageQuotaExceeded","listed":1092,"speed":32928.24,"totalBytes":2399924.0,"totalChecks":6,"totalTransfers":0},"source":"accounting/stats.go:551"}"#;
        match parse_rclone_line(line) {
            ParsedRcloneLine::Progress {
                current,
                max,
                last_error,
            } => {
                assert_eq!(current, 2_399_924.0);
                assert_eq!(max, 2_399_924.0);
                assert!(last_error.is_some_and(|e| is_quota_exceeded(&e)));
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn copied_and_skipped_lines_still_report_changes() {
        let copied = r#"{"time":"2026-10-12T01:14:50.0+05:00","level":"info","msg":"Copied (new)","object":"Baldur's Gate 3/mapping.yaml","objectType":"*drive.Object","source":"operations/operations.go:2591"}"#;
        match parse_rclone_line(copied) {
            ParsedRcloneLine::Change { msg, .. } => assert_eq!(msg, "Copied (new)"),
            other => panic!("expected a change, got {other:?}"),
        }

        let skipped = r#"{"time":"2026-10-12T01:14:50.0+05:00","level":"info","msg":"Skipped copy as already exists","object":"Ultrakill/mapping.yaml","objectType":"*drive.Object","skipped":"copy","source":"operations/operations.go:2591"}"#;
        match parse_rclone_line(skipped) {
            ParsedRcloneLine::Skip { skipped, .. } => assert_eq!(skipped, "copy"),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_lines_are_ignored_rather_than_misread() {
        // rclone only emits JSON when `--use-json-log` is set; anything else must not
        // be mistaken for a diagnostic.
        assert!(matches!(parse_rclone_line("Transferred: 1 / 1, 100%"), ParsedRcloneLine::Ignored));
        assert!(matches!(parse_rclone_line(""), ParsedRcloneLine::Ignored));
    }
}
