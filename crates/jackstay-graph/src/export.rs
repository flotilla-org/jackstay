//! Spawning and watching bridge halves from a coordinator.
//!
//! The halves are plain processes (`jackstay-bridge egress` and `ingress`).
//! They print one JSON object per line on stdout as they progress; this module
//! launches them, reads those lines, and exposes the current phase, the codec
//! decision and the final report to whoever holds the handle. Nothing here
//! knows about capture sessions or hosts; that is the coordinator's business.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{ChromaPolicy, CodecDecision};

/// A status line a half prints on stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HalfEvent {
    /// A listening half bound one of its sockets.
    Listening { path: String },
    /// Both halves said hello and agreed on a codec.
    Ready { decision: CodecDecision },
    /// The ingress half's republished publication can be attached to.
    PublicationUp { service: String, token: Option<String> },
    /// The half ended; `report` is its `EgressReport` or `IngressReport`.
    Report { report: serde_json::Value },
    /// The half failed before or after running.
    Failed { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Listening,
    Running,
    Ended,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HalfStatus {
    pub phase: Option<Phase>,
    pub decision: Option<CodecDecision>,
    pub publication: Option<(String, Option<String>)>,
    pub report: Option<serde_json::Value>,
    pub failure: Option<String>,
    pub exit_code: Option<i32>,
}

impl HalfStatus {
    fn apply(&mut self, event: &HalfEvent) {
        match event {
            HalfEvent::Listening { .. } => self.phase = Some(Phase::Listening),
            HalfEvent::Ready { decision } => {
                self.phase = Some(Phase::Running);
                self.decision = Some(decision.clone());
            }
            HalfEvent::PublicationUp { service, token } => {
                self.phase = Some(Phase::Running);
                self.publication = Some((service.clone(), token.clone()));
            }
            HalfEvent::Report { report } => {
                self.phase = Some(Phase::Ended);
                self.report = Some(report.clone());
            }
            HalfEvent::Failed { message } => {
                self.phase = Some(Phase::Ended);
                self.failure = Some(message.clone());
            }
        }
    }
}

/// Where the `jackstay-bridge` executable is. Explicit path, then the
/// `JACKSTAY_BRIDGE_BIN` variable, then the PATH.
#[derive(Debug, Clone)]
pub struct BridgeBinary(pub PathBuf);

impl BridgeBinary {
    pub fn locate(explicit: Option<&Path>) -> Option<Self> {
        if let Some(p) = explicit {
            return Some(Self(p.to_path_buf()));
        }
        if let Ok(p) = std::env::var("JACKSTAY_BRIDGE_BIN") {
            return Some(Self(PathBuf::from(p)));
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join("jackstay-bridge"))
            .find(|p| p.is_file())
            .map(Self)
    }
}

#[derive(Debug, Clone)]
pub struct EgressSpec {
    /// The producer-side Mach service the egress attaches to.
    pub source_service: String,
    pub source_token: Option<String>,
    /// Sockets the egress listens on for the peer; the coordinator forwards them.
    pub media_socket: PathBuf,
    pub control_socket: PathBuf,
    pub link_token: String,
    pub chroma: ChromaPolicy,
    pub bitrate_bps: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct IngressSpec {
    /// Sockets the ingress connects to: the local ends of the forward.
    pub media_socket: PathBuf,
    pub control_socket: PathBuf,
    /// Mach service name the republished publication is vended under.
    pub service: String,
    pub viewer_token: Option<String>,
    pub link_token: String,
    pub chroma: ChromaPolicy,
}

fn chroma_arg(policy: ChromaPolicy) -> &'static str {
    match policy {
        ChromaPolicy::Require444 => "require444",
        ChromaPolicy::Prefer444 => "prefer444",
        ChromaPolicy::Any => "any",
    }
}

/// Command line for an egress half that listens for its peer.
#[must_use]
pub fn egress_arguments(spec: &EgressSpec) -> Vec<String> {
    let mut args = vec![
        "egress".to_owned(),
        "--listen".to_owned(),
        "--media".to_owned(),
        spec.media_socket.to_string_lossy().into_owned(),
        "--control".to_owned(),
        spec.control_socket.to_string_lossy().into_owned(),
        "--service".to_owned(),
        spec.source_service.clone(),
        "--link-token".to_owned(),
        spec.link_token.clone(),
        "--chroma".to_owned(),
        chroma_arg(spec.chroma).to_owned(),
    ];
    if let Some(t) = &spec.source_token {
        args.push("--source-token".to_owned());
        args.push(t.clone());
    }
    if let Some(b) = spec.bitrate_bps {
        args.push("--bitrate".to_owned());
        args.push(b.to_string());
    }
    args
}

/// Command line for an ingress half that connects to its peer and vends a service.
#[must_use]
pub fn ingress_arguments(spec: &IngressSpec) -> Vec<String> {
    let mut args = vec![
        "ingress".to_owned(),
        "--media".to_owned(),
        spec.media_socket.to_string_lossy().into_owned(),
        "--control".to_owned(),
        spec.control_socket.to_string_lossy().into_owned(),
        "--service".to_owned(),
        spec.service.clone(),
        "--link-token".to_owned(),
        spec.link_token.clone(),
        "--chroma".to_owned(),
        chroma_arg(spec.chroma).to_owned(),
    ];
    if let Some(t) = &spec.viewer_token {
        args.push("--viewer-token".to_owned());
        args.push(t.clone());
    }
    args
}

/// Parses one stdout line; lines that are not status objects are ignored.
#[must_use]
pub fn parse_event(line: &str) -> Option<HalfEvent> {
    serde_json::from_str(line.trim()).ok()
}

/// A half running as a direct child with its stdout read on a thread.
#[derive(Debug)]
pub struct HalfHandle {
    child: Child,
    status: Arc<Mutex<HalfStatus>>,
}

impl HalfHandle {
    fn spawn(binary: &BridgeBinary, args: &[String]) -> std::io::Result<Self> {
        let mut child = Command::new(&binary.0)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let status = Arc::new(Mutex::new(HalfStatus {
            phase: Some(Phase::Starting),
            ..HalfStatus::default()
        }));
        if let Some(stdout) = child.stdout.take() {
            let status = status.clone();
            std::thread::Builder::new().name("jackstay-graph-half".into()).spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(event) = parse_event(&line) {
                        if let Ok(mut s) = status.lock() {
                            s.apply(&event);
                        }
                    }
                }
            })?;
        }
        Ok(Self { child, status })
    }

    #[must_use]
    pub fn status(&mut self) -> HalfStatus {
        let mut s = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        if let Ok(Some(exit)) = self.child.try_wait() {
            s.exit_code = exit.code();
            if s.phase != Some(Phase::Ended) {
                s.phase = Some(Phase::Ended);
            }
        }
        s
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits until the half reports the requested phase or later, the process
    /// exits, or `timeout` passes.
    pub fn wait_for(&mut self, phase: Phase, timeout: Duration) -> HalfStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.status();
            let reached = matches!(
                (s.phase, phase),
                (Some(Phase::Ended), _)
                    | (Some(Phase::Running), Phase::Running | Phase::Listening | Phase::Starting)
                    | (Some(Phase::Listening), Phase::Listening | Phase::Starting)
                    | (Some(Phase::Starting), Phase::Starting)
            );
            if reached || Instant::now() >= deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Asks the half to stop (SIGTERM on Unix) and waits briefly; kills it if
    /// it does not exit.
    pub fn stop(&mut self) -> HalfStatus {
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            // SAFETY: signalling our own child.
            unsafe {
                kill(self.child.id() as i32, 15);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return self.status();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.status()
    }
}

impl Drop for HalfHandle {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.stop();
        }
    }
}

/// Spawns an egress half listening on the spec's sockets.
pub fn spawn_egress(binary: &BridgeBinary, spec: &EgressSpec) -> std::io::Result<HalfHandle> {
    HalfHandle::spawn(binary, &egress_arguments(spec))
}

/// An ingress half running as a launchd job, with its status read from the
/// job's stdout file.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct IngressJob {
    job: crate::launchd::LaunchdJob,
    status: HalfStatus,
    read_bytes: u64,
}

#[cfg(target_os = "macos")]
impl IngressJob {
    /// Registers a launchd job named `spec.service` in `directory` running the
    /// ingress half.
    pub fn spawn(binary: &BridgeBinary, spec: &IngressSpec, directory: &Path) -> Result<Self, crate::launchd::LaunchdError> {
        let mut program = vec![binary.0.to_string_lossy().into_owned()];
        program.extend(ingress_arguments(spec));
        let job = crate::launchd::LaunchdJob::bootstrap(&spec.service, directory, &program)?;
        Ok(Self {
            job,
            status: HalfStatus {
                phase: Some(Phase::Starting),
                ..HalfStatus::default()
            },
            read_bytes: 0,
        })
    }

    /// Reads any new status lines from the job's stdout file.
    pub fn status(&mut self) -> HalfStatus {
        if let Ok(content) = std::fs::read(self.job.stdout_path()) {
            let start = usize::try_from(self.read_bytes).unwrap_or(0).min(content.len());
            let fresh = &content[start..];
            if let Some(last_newline) = fresh.iter().rposition(|b| *b == b'\n') {
                let complete = &fresh[..=last_newline];
                for line in String::from_utf8_lossy(complete).lines() {
                    if let Some(event) = parse_event(line) {
                        self.status.apply(&event);
                    }
                }
                self.read_bytes += complete.len() as u64;
            }
        }
        if self.status.phase != Some(Phase::Ended) && !self.job.is_registered() {
            self.status.phase = Some(Phase::Ended);
        }
        self.status.clone()
    }

    pub fn wait_for_publication(&mut self, timeout: Duration) -> HalfStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.status();
            if s.publication.is_some() || s.phase == Some(Phase::Ended) || Instant::now() >= deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[must_use]
    pub fn service(&self) -> &str {
        self.job.label()
    }

    #[must_use]
    pub fn stderr_path(&self) -> PathBuf {
        self.job.stderr_path()
    }

    pub fn stop(&mut self) -> Result<(), crate::launchd::LaunchdError> {
        self.job.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chroma, Codec};

    #[test]
    fn events_round_trip_and_drive_status() {
        let decision = CodecDecision {
            codec: Codec::Hevc,
            chroma: Chroma::Full,
            hardware: true,
            reason: "test".into(),
        };
        let lines = [
            serde_json::to_string(&HalfEvent::Listening { path: "/tmp/m".into() }).unwrap(),
            "human readable noise".to_owned(),
            serde_json::to_string(&HalfEvent::Ready {
                decision: decision.clone(),
            })
            .unwrap(),
            serde_json::to_string(&HalfEvent::Report {
                report: serde_json::json!({"frames_sent": 3}),
            })
            .unwrap(),
        ];
        let mut status = HalfStatus::default();
        for l in &lines {
            if let Some(e) = parse_event(l) {
                status.apply(&e);
            }
        }
        assert_eq!(status.phase, Some(Phase::Ended));
        assert_eq!(status.decision, Some(decision));
        assert_eq!(status.report.unwrap()["frames_sent"], 3);
    }

    #[test]
    fn argument_lines_carry_every_field() {
        let e = egress_arguments(&EgressSpec {
            source_service: "svc".into(),
            source_token: Some("tok".into()),
            media_socket: "/m".into(),
            control_socket: "/c".into(),
            link_token: "L".into(),
            chroma: ChromaPolicy::Require444,
            bitrate_bps: Some(5),
        });
        assert_eq!(
            e,
            [
                "egress",
                "--listen",
                "--media",
                "/m",
                "--control",
                "/c",
                "--service",
                "svc",
                "--link-token",
                "L",
                "--chroma",
                "require444",
                "--source-token",
                "tok",
                "--bitrate",
                "5"
            ]
        );
        let i = ingress_arguments(&IngressSpec {
            media_socket: "/m".into(),
            control_socket: "/c".into(),
            service: "dst".into(),
            viewer_token: None,
            link_token: "L".into(),
            chroma: ChromaPolicy::Any,
        });
        assert!(i.contains(&"--service".to_owned()) && !i.contains(&"--viewer-token".to_owned()));
    }
}
