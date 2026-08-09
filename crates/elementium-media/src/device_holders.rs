//! Which process is holding a device open.
//!
//! "Device or resource busy" is true and useless. A camera that will not start is nearly
//! always a camera another application already has, and the operating system knows exactly
//! which one -- so a report that stops at the errno makes the user carry the question to
//! someone who can read `/proc`, when the answer was available at the moment it failed.
//!
//! Deliberately reads only two things about a process: its `comm`, and the application name
//! its cgroup path already carries. Not `cmdline`, which routinely holds access tokens and
//! session identifiers and has no business in a log written to diagnose a busy webcam.

/// A process holding a device node open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHolder {
    /// The device node, e.g. `/dev/video5`.
    pub device: String,
    pub pid: u32,
    /// The kernel's short name for the process, e.g. `electron`.
    pub process: String,
    /// The application the process belongs to, when its cgroup names one.
    ///
    /// This is what makes the message worth printing: a dozen desktop applications are
    /// called `electron`, and exactly one of them is the one holding the camera.
    pub app: Option<String>,
}

/// Process names that identify a runtime rather than an application.
///
/// For these the cgroup's application name is worth more than `comm`: a dozen programs on
/// a desktop are called `electron`, and exactly one of them has the camera.
const GENERIC_RUNTIMES: [&str; 6] = ["electron", "chrome", "chromium", "node", "python3", "java"];

impl DeviceHolder {
    /// How to name this holder to a person.
    ///
    /// `comm` first, unless it names a runtime instead of a program. Measured against a
    /// real desktop rather than assumed: Signal's capture process is `electron` and its
    /// cgroup says `signal`, while OBS's is `.obs-wrapped` and its cgroup says
    /// `niri-tofi-drun` -- the launcher that happened to start it. Neither source is right
    /// on its own.
    #[must_use]
    pub fn describe(&self) -> String {
        let process = tidy_process_name(&self.process);
        let generic = GENERIC_RUNTIMES.contains(&process.as_str());
        match (&self.app, generic) {
            (Some(app), true) => format!("{app} (pid {}, {process})", self.pid),
            _ => format!("{process} (pid {})", self.pid),
        }
    }
}

/// Strip the decorations a packaged binary picks up: nix wrappers are `.foo-wrapped`.
fn tidy_process_name(comm: &str) -> String {
    comm.trim_start_matches('.')
        .trim_end_matches("-wrapped")
        .to_owned()
}

/// Every process holding open a device whose path starts with `prefix`, excluding this one.
///
/// Only processes this user owns are visible, which is the case that matters: a camera is
/// taken by something on the same desktop. Anything unreadable is skipped rather than
/// reported as an error -- the point is to add detail to a failure, never to become one.
#[must_use]
pub fn holders_of(prefix: &str) -> Vec<DeviceHolder> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = prefix;
        Vec::new()
    }
    #[cfg(target_os = "linux")]
    {
        linux::holders_of(prefix)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::DeviceHolder;

    /// Processes that hold camera nodes as part of managing them.
    const MEDIA_SERVERS: [&str; 3] = ["pipewire", "pipewire-pulse", "wireplumber"];

    pub fn holders_of(prefix: &str) -> Vec<DeviceHolder> {
        let own = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut holders = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            if pid == own {
                continue;
            }
            let process = process_name(pid);
            // PipeWire holds every camera node it knows about, to enumerate their formats.
            // It is the thing that shares a camera out, never the thing preventing it, and
            // naming it would send someone to fix the one process that is behaving.
            if MEDIA_SERVERS.contains(&process.as_str()) {
                continue;
            }
            for device in devices_held_by(pid, prefix) {
                holders.push(DeviceHolder {
                    device,
                    pid,
                    process: process.clone(),
                    app: app_name(pid),
                });
            }
        }
        holders.sort_by(|a, b| (&a.device, a.pid).cmp(&(&b.device, b.pid)));
        holders.dedup();
        holders
    }

    /// The device paths `pid` has open that begin with `prefix`.
    fn devices_held_by(pid: u32, prefix: &str) -> Vec<String> {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            // Almost always another user's process, or one that exited while we looked.
            return Vec::new();
        };
        let mut found: Vec<String> = fds
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .filter_map(|target| target.to_str().map(str::to_owned))
            .filter(|target| target.starts_with(prefix))
            // A node that has been removed reads back as `/dev/video0 (deleted)`. It is a
            // device that no longer exists, so nobody is competing for it and saying so
            // would be worse than saying nothing.
            .filter(|target| !target.ends_with(" (deleted)"))
            .collect();
        found.sort();
        found.dedup();
        found
    }

    fn process_name(pid: u32) -> String {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map_or_else(|_| "unknown".to_owned(), |name| name.trim().to_owned())
    }

    /// The application name a systemd user session puts in the cgroup path.
    ///
    /// A desktop application launched from a `.desktop` file lands in a scope named after
    /// it: `/user.slice/.../app.slice/app-signal-6594.scope` is Signal. That is how a
    /// process whose `comm` is the useless `electron` still gets named.
    fn app_name(pid: u32) -> Option<String> {
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let scope = cgroup
            .lines()
            .filter_map(|line| line.rsplit('/').next())
            .find(|segment| segment.starts_with("app-"))?;
        let name = scope
            .trim_end_matches(".scope")
            .trim_end_matches(".service")
            .strip_prefix("app-")?;
        // The launcher appends `-<pid>`, and some desktops escape the name with `\x2d`.
        let name = name
            .rsplit_once('-')
            .filter(|(_, tail)| tail.chars().all(|c| c.is_ascii_digit()))
            .map_or(name, |(head, _)| head);
        let cleaned = name.replace("\\x2d", "-");
        (!cleaned.is_empty()).then_some(cleaned)
    }

    #[cfg(test)]
    mod tests {
        use super::{app_name, devices_held_by, holders_of, process_name};

        /// This process holds its own executable and standard streams, and none of them is
        /// a camera -- so a prefix nothing matches must come back empty rather than
        /// guessing or failing.
        #[test]
        fn a_prefix_nothing_matches_finds_nothing() {
            assert!(holders_of("/dev/definitely-not-a-real-device").is_empty());
        }

        /// A pid that does not exist must be silent, not an error: processes come and go
        /// while the scan runs, and that is normal rather than exceptional.
        #[test]
        fn a_process_that_has_gone_is_skipped() {
            // No process has this id: the kernel's maximum is far below it.
            assert!(devices_held_by(u32::MAX, "/dev/").is_empty());
            assert_eq!(process_name(u32::MAX), "unknown");
            assert_eq!(app_name(u32::MAX), None);
        }

        /// The scan must see something real, or it is not testing anything: every process
        /// has `/proc/<pid>/fd` entries pointing at the tty or the null device.
        #[test]
        fn the_scan_reaches_real_file_descriptors() {
            let own = devices_held_by(std::process::id(), "/");
            assert!(
                !own.is_empty(),
                "this process must hold at least one open path"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceHolder, tidy_process_name};

    /// Measured on a real desktop: nix wraps binaries and the wrapper's name is what `comm`
    /// reports, so OBS arrives as `.obs-wrapped`.
    #[test]
    fn a_wrapped_binary_is_named_by_the_program() {
        assert_eq!(tidy_process_name(".obs-wrapped"), "obs");
        assert_eq!(tidy_process_name("electron"), "electron");
    }

    /// The cgroup is only worth consulting when `comm` names a runtime.
    ///
    /// OBS's scope was `app-niri-tofi-drun-...`, after the launcher that started it. Taking
    /// the cgroup name unconditionally reported the camera as held by a program picker.
    #[test]
    fn a_misleading_cgroup_does_not_override_a_real_process_name() {
        let holder = DeviceHolder {
            device: "/dev/video11".to_owned(),
            pid: 80_262,
            process: ".obs-wrapped".to_owned(),
            app: Some("niri-tofi-drun".to_owned()),
        };
        assert_eq!(holder.describe(), "obs (pid 80262)");
    }

    /// A holder with a known application is named by it: `electron (pid 123)` tells nobody
    /// anything, and naming the application is the whole point.
    #[test]
    fn a_named_application_leads_the_description() {
        let holder = DeviceHolder {
            device: "/dev/video5".to_owned(),
            pid: 3_830_617,
            process: "electron".to_owned(),
            app: Some("signal".to_owned()),
        };
        assert_eq!(holder.describe(), "signal (pid 3830617, electron)");
    }

    /// Without one, the process name still beats nothing.
    #[test]
    fn an_unnamed_application_falls_back_to_the_process() {
        let holder = DeviceHolder {
            device: "/dev/video5".to_owned(),
            pid: 42,
            process: "obs".to_owned(),
            app: None,
        };
        assert_eq!(holder.describe(), "obs (pid 42)");
    }
}
