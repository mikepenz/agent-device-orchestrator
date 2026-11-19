//! `devq service install|uninstall|status`: launchd LaunchAgent running `devq live` (macOS only).
use clap::Subcommand;

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.devq.live";

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Write the LaunchAgent plist and load it
    Install {
        /// Port for `devq live`
        #[arg(long, default_value_t = 7878)]
        port: u16,
    },
    /// Restart the running service (use after upgrading the binary)
    Restart,
    /// Unload the LaunchAgent and delete its plist
    Uninstall,
    /// Show plist, launchd state and whether the dashboard answers
    Status {
        /// Port to probe
        #[arg(long, default_value_t = 7878)]
        port: u16,
    },
}

#[cfg(not(target_os = "macos"))]
pub fn run(_: ServiceCmd) -> crate::db::Res<i32> {
    eprintln!("service management is only implemented for macOS (launchd); run `devq live` under systemd --user or similar");
    Ok(2)
}

#[cfg(target_os = "macos")]
pub use mac::run;

#[cfg(target_os = "macos")]
mod mac {
    use super::{ServiceCmd, LABEL};
    use crate::db::Res;
    use crate::pool;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::process::Command;

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
    }

    fn plist(exe: &str, port: u16, log: &str, env: &[(String, String)]) -> String {
        let args: String =
            [exe, "live", "--port", &port.to_string()].iter().map(|a| format!("    <string>{}</string>\n", esc(a))).collect();
        let envs: String = env.iter().map(|(k, v)| format!("    <key>{}</key><string>{}</string>\n", esc(k), esc(v))).collect();
        format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>StandardOutPath</key><string>{l}</string>\n  <key>StandardErrorPath</key><string>{l}</string>\n  <key>EnvironmentVariables</key>\n  <dict>\n{envs}  </dict>\n</dict>\n</plist>\n",
        l = esc(log)
    )
    }

    /// Parses `launchctl print` output: (state, pid).
    fn parse_print(out: &str) -> (Option<String>, Option<u32>) {
        let field = |k: &str| out.lines().find_map(|l| l.trim().strip_prefix(k)).map(|v| v.trim().to_string());
        (field("state = "), field("pid = ").and_then(|p| p.parse().ok()))
    }

    fn plist_path() -> Res<PathBuf> {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    fn uid() -> u32 {
        crate::sys::uid()
    }

    fn wait_unloaded() {
        for _ in 0..30 {
            if loaded().is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
        Command::new("launchctl").args(args).output()
    }

    fn loaded() -> Option<String> {
        launchctl(&["print", &format!("gui/{}/{LABEL}", uid())])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn probe(port: u16, path: &str) -> bool {
        let Ok(mut s) =
            std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), std::time::Duration::from_secs(2))
        else {
            return false;
        };
        let t = Some(std::time::Duration::from_secs(5));
        let _ = (s.set_read_timeout(t), s.set_write_timeout(t));
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        let mut buf = String::new();
        s.write_all(req.as_bytes()).is_ok() && s.read_to_string(&mut buf).is_ok() && buf.starts_with("HTTP/1.1 200")
    }

    pub fn run(cmd: ServiceCmd) -> Res<i32> {
        let path = plist_path()?;
        let domain = format!("gui/{}", uid());
        let target = format!("{domain}/{LABEL}");
        match cmd {
            ServiceCmd::Install { port } => {
                // not canonicalized: symlinked installs (Homebrew) must survive upgrades
                let exe = std::env::current_exe()?.to_string_lossy().into_owned();
                if exe.contains("/target/") {
                    eprintln!("warning: {exe} is inside target/ and may move or be rebuilt; run `cargo install --path .` and install again from that binary");
                }
                let home = std::path::absolute(pool::devq_home())?;
                let log = home.join("live.log").to_string_lossy().into_owned();
                let mut env = vec![];
                if let Some(p) = std::env::var("PATH").ok().filter(|p| !p.is_empty()) {
                    env.push(("PATH".to_string(), p));
                }
                if std::env::var_os("DEVQ_HOME").is_some() {
                    env.push(("DEVQ_HOME".into(), home.to_string_lossy().into_owned()));
                }
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(&path, plist(&exe, port, &log, &env))?;
                if loaded().is_some() {
                    let _ = launchctl(&["bootout", &target]);
                    wait_unloaded();
                }
                let mut o = launchctl(&["bootstrap", &domain, &path.to_string_lossy()])?;
                if !o.status.success() && o.status.code() == Some(5) {
                    // ponytail: launchd can still be tearing down the old job; one retry
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    o = launchctl(&["bootstrap", &domain, &path.to_string_lossy()])?;
                }
                if !o.status.success() {
                    return Err(format!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&o.stderr).trim()).into());
                }
                println!("installed {}\ndashboard: http://127.0.0.1:{port}/  (log: {log})", path.display());
                let up = (0..15).any(|_| {
                    probe(port, "/manifest.webmanifest") || {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        false
                    }
                });
                if !up {
                    eprintln!("warning: nothing answered on 127.0.0.1:{port} within 3s; check {log} (something else may already be bound to the port)");
                }
                println!("install as an app: Chrome -> menu -> \"Install devq live\"; Safari -> File -> Add to Dock");
            }
            ServiceCmd::Restart => {
                let o = launchctl(&["kickstart", "-k", &target])?;
                if !o.status.success() {
                    return Err(
                        format!("launchctl kickstart failed (is it installed?): {}", String::from_utf8_lossy(&o.stderr).trim()).into()
                    );
                }
                println!("restarted {LABEL}");
            }
            ServiceCmd::Uninstall => {
                let _ = launchctl(&["bootout", &target]);
                match std::fs::remove_file(&path) {
                    Ok(()) => println!("uninstalled {}", path.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not installed"),
                    Err(e) => return Err(e.into()),
                }
            }
            ServiceCmd::Status { port } => {
                println!("plist:  {} ({})", path.display(), if path.exists() { "present" } else { "missing" });
                match loaded() {
                    Some(out) => {
                        let (state, pid) = parse_print(&out);
                        println!(
                            "loaded: yes, state {}, pid {}",
                            state.as_deref().unwrap_or("?"),
                            pid.map_or("-".into(), |p| p.to_string())
                        );
                    }
                    None => println!("loaded: no"),
                }
                println!(
                    "http:   {}",
                    if probe(port, "/manifest.webmanifest") {
                        format!("http://127.0.0.1:{port}/ answers 200")
                    } else {
                        format!("no 200 from 127.0.0.1:{port}")
                    }
                );
            }
        }
        Ok(0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_args_and_escaping() {
            let p = plist("/a b/devq&<x>", 7891, "/h/live.log", &[("PATH".into(), "/x:/y\"'".into()), ("DEVQ_HOME".into(), "/t".into())]);
            assert!(p.contains("<string>/a b/devq&amp;&lt;x&gt;</string>\n    <string>live</string>\n    <string>--port</string>\n    <string>7891</string>"));
            assert!(p.contains("<key>PATH</key><string>/x:/y&quot;&apos;</string>"));
            assert!(p.contains("<key>DEVQ_HOME</key><string>/t</string>"));
            assert!(p.contains("<key>KeepAlive</key><true/>") && p.contains("<key>RunAtLoad</key><true/>"));
            assert_eq!(p.matches("/h/live.log").count(), 2);
        }

        #[test]
        fn parses_launchctl_print() {
            let out = "gui/501/dev.devq.live = {\n\tactive count = 1\n\tstate = running\n\tprogram = /x\n\tpid = 4242\n}\n";
            assert_eq!(parse_print(out), (Some("running".into()), Some(4242)));
            assert_eq!(parse_print("\tstate = waiting\n"), (Some("waiting".into()), None));
        }
    }
}
