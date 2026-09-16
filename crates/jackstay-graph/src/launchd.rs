//! A launchd job for a process that must vend a Mach service.
//!
//! On macOS a named XPC listener is only reachable when launchd registered the
//! name for the job that owns it, so an ingress half that viewers attach to by
//! name runs as a launchd job rather than as a plain child. The job is
//! bootstrapped into the user's GUI domain from a generated plist and booted
//! out on drop. Its stdout and stderr go to files in the job directory so a
//! coordinator can read the half's status lines.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, thiserror::Error)]
pub enum LaunchdError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("launchctl {verb}: {stderr}")]
    Launchctl { verb: &'static str, stderr: String },
    #[error("path is not valid UTF-8: {0}")]
    Path(PathBuf),
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn utf8(path: &Path) -> Result<&str, LaunchdError> {
    path.to_str().ok_or_else(|| LaunchdError::Path(path.to_path_buf()))
}

fn gui_domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc_getuid() })
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(not(unix))]
unsafe fn libc_getuid() -> u32 {
    0
}

/// A bootstrapped job. Dropping it boots the job out.
#[derive(Debug)]
pub struct LaunchdJob {
    label: String,
    target: String,
    directory: PathBuf,
    stopped: bool,
}

impl LaunchdJob {
    /// Writes `job.plist` into `directory` and bootstraps it. `program` is the
    /// executable followed by its arguments. The job's Mach service is named
    /// after `label`.
    pub fn bootstrap(label: &str, directory: &Path, program: &[String]) -> Result<Self, LaunchdError> {
        std::fs::create_dir_all(directory)?;
        let plist = directory.join("job.plist");
        let arguments: String = program.iter().map(|a| format!("<string>{}</string>", xml(a))).collect();
        std::fs::write(
            &plist,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array>{arguments}</array>
<key>RunAtLoad</key><true/>
<key>MachServices</key><dict><key>{label}</key><true/></dict>
<key>StandardOutPath</key><string>{stdout}</string><key>StandardErrorPath</key><string>{stderr}</string>
</dict></plist>"#,
                label = xml(label),
                stdout = xml(utf8(&directory.join("stdout.log"))?),
                stderr = xml(utf8(&directory.join("stderr.log"))?),
            ),
        )?;
        let domain = gui_domain();
        let result = Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist).output()?;
        if !result.status.success() {
            return Err(LaunchdError::Launchctl {
                verb: "bootstrap",
                stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            });
        }
        Ok(Self {
            label: label.to_owned(),
            target: format!("{domain}/{label}"),
            directory: directory.to_path_buf(),
            stopped: false,
        })
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn stdout_path(&self) -> PathBuf {
        self.directory.join("stdout.log")
    }

    #[must_use]
    pub fn stderr_path(&self) -> PathBuf {
        self.directory.join("stderr.log")
    }

    /// Whether launchd still has the job.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        Command::new("launchctl")
            .args(["print", &self.target])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Boots the job out explicitly. Idempotent.
    pub fn stop(&mut self) -> Result<(), LaunchdError> {
        if self.stopped {
            return Ok(());
        }
        let result = Command::new("launchctl").args(["bootout", &self.target]).output()?;
        self.stopped = true;
        if !result.status.success() && self.is_registered() {
            return Err(LaunchdError::Launchctl {
                verb: "bootout",
                stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            });
        }
        Ok(())
    }
}

impl Drop for LaunchdJob {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
