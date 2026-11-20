use crate::db::{self, Res};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SYNC_THROTTLE_MS: i64 = 3_000;

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub pool: PoolCfg,
    pub live: LiveCfg,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct LiveCfg {
    pub port: Option<u16>,
}

pub const DEFAULT_PORT: u16 = 47878;

/// Flag or `DEVQ_PORT` (already merged by clap), then `[live] port` in config.toml, then the default.
pub fn live_port(flag: Option<u16>) -> Res<u16> {
    Ok(flag.or(load_config(&devq_home())?.live.port).unwrap_or(DEFAULT_PORT))
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct PoolCfg {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    #[serde(rename = "static")]
    pub statics: Vec<String>,
}

/// `$DEVQ_HOME` or `~/.devq`, created if missing.
pub fn devq_home() -> PathBuf {
    let p = match std::env::var_os("DEVQ_HOME").filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => crate::sys::home_dir()
            .unwrap_or_else(|e| {
                eprintln!("devq: {e}");
                std::process::exit(2);
            })
            .join(".devq"),
    };
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Missing file = defaults; a malformed file is an error.
pub fn load_config(home: &Path) -> Res<Config> {
    match std::fs::read_to_string(home.join("config.toml")) {
        Ok(s) => Ok(toml::from_str(&s)?),
        Err(_) => Ok(Config::default()),
    }
}

#[derive(Debug, PartialEq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
    pub model: String,
}

/// Parses `adb devices -l` output.
pub fn parse_adb(out: &str) -> Vec<AdbDevice> {
    const STATES: [&str; 8] = ["device", "offline", "unauthorized", "recovery", "bootloader", "sideload", "host", "no permissions"];
    out.lines()
        .filter(|l| !l.starts_with("List of devices") && !l.starts_with('*'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let rest: Vec<&str> = it.collect();
            // state is everything before the first key:value field ("no permissions" has two words)
            let n = rest.iter().position(|f| f.contains(':')).unwrap_or(rest.len());
            let state = rest[..n].join(" ");
            if !STATES.iter().any(|s| state == *s || state.starts_with(&format!("{s} ("))) {
                return None;
            }
            let model = rest.iter().find_map(|f| f.strip_prefix("model:")).unwrap_or("").to_string();
            Some(AdbDevice { serial, state, model })
        })
        .collect()
}

/// `*` wildcard match, anchored at both ends.
pub fn glob(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((head, rest)) => {
            let Some(s) = s.strip_prefix(head) else { return false };
            (0..=s.len()).any(|i| s.is_char_boundary(i) && glob(rest, &s[i..]))
        }
    }
}

/// Drops devices hidden by config (see `visible`) from a `state_json` document.
pub fn filter_state(st: &mut serde_json::Value, cfg: &Config) {
    for k in ["devices", "per_device"] {
        if let Some(a) = st[k].as_array_mut() {
            a.retain(|d| visible(cfg, d["serial"].as_str().unwrap_or("")));
        }
    }
}

/// Shown to users: allowed by include/exclude, or a configured static.
pub fn visible(cfg: &Config, serial: &str) -> bool {
    allowed(&cfg.pool, serial) || cfg.pool.statics.iter().any(|s| s == serial)
}

fn allowed(cfg: &PoolCfg, serial: &str) -> bool {
    (cfg.include.is_empty() || cfg.include.iter().any(|p| glob(p, serial))) && !cfg.exclude.iter().any(|p| glob(p, serial))
}

/// `None` when adb itself failed (spawn error / non-zero exit).
fn adb_devices() -> Option<Vec<AdbDevice>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut ch = std::process::Command::new("adb")
        .args(["devices", "-l"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = ch.stdout.take()?;
    // drain on a thread so a full pipe can't stall the child
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let start = std::time::Instant::now();
    let ok = loop {
        match ch.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = ch.kill();
                let _ = ch.wait();
                break false;
            }
        }
    };
    // A daemon may inherit the pipe and keep it open: bounded wait, reader thread abandoned on timeout.
    let s = rx.recv_timeout(Duration::from_secs(1)).ok()?;
    ok.then(|| parse_adb(&s))
}

/// Throttled (3s) adb sync. Call outside a transaction; it opens its own.
pub fn sync(conn: &mut Connection, now: i64, cfg: &Config) -> Res<()> {
    let last: Option<i64> = conn.query_row("SELECT v FROM meta WHERE k='last_sync'", [], |r| r.get(0)).ok();
    if last.is_some_and(|l| now - l < SYNC_THROTTLE_MS && now >= l) {
        return Ok(());
    }
    let devs = adb_devices();
    db::immediate(conn, |c| {
        apply(c, now, devs.as_deref(), &cfg.pool)?;
        c.execute("INSERT OR REPLACE INTO meta VALUES('last_sync',?)", [now])?;
        Ok(())
    })
}

/// Marks everything offline, then upserts allowed adb devices and statics.
/// `devs == None` (adb failed) leaves android rows untouched.
fn apply(conn: &Connection, now: i64, devs: Option<&[AdbDevice]>, cfg: &PoolCfg) -> Res<()> {
    if devs.is_some() {
        conn.execute("UPDATE devices SET state='offline'", [])?;
    } else {
        // adb failed: keep android rows as they were, but never leave config-disallowed devices grantable.
        let rows: Vec<(String, String)> =
            conn.prepare("SELECT serial,kind FROM devices")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        for (s, k) in rows {
            let stale = if k == "static" { !cfg.statics.contains(&s) } else { !allowed(cfg, &s) };
            if stale {
                conn.execute("UPDATE devices SET state='offline' WHERE serial=?", [s])?;
            }
        }
    }
    let up = |serial: &str, kind: &str, model: &str, state: &str| {
        conn.execute(
            "INSERT INTO devices(serial,kind,model,state,first_seen,last_seen) VALUES(?1,?2,?3,?4,?5,?5)
             ON CONFLICT(serial) DO UPDATE SET kind=?2, state=?4, last_seen=?5,
               model=CASE WHEN ?3!='' THEN ?3 ELSE model END",
            params![serial, kind, model, state, now],
        )
    };
    for d in devs.unwrap_or(&[]).iter().filter(|d| allowed(cfg, &d.serial)) {
        let state = if d.state == "device" { "online" } else { "offline" };
        up(&d.serial, "android", &d.model, state)?;
    }
    for s in &cfg.statics {
        up(s, "static", "", "online")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "List of devices attached\n\
emulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\n\
\n\
R5CT1234ABC            unauthorized transport_id:2\n\
0123456789             offline\n\
* daemon started successfully\n";

    #[test]
    fn parse_adb_lines() {
        let d = parse_adb(OUT);
        assert_eq!(d.len(), 3);
        assert_eq!((d[0].serial.as_str(), d[0].state.as_str(), d[0].model.as_str()), ("emulator-5554", "device", "sdk_gphone64_arm64"));
        assert_eq!(d[1].state, "unauthorized");
        assert_eq!(d[2].state, "offline");
        assert!(parse_adb("").is_empty());
        let banner = "adb server version (41) doesn't match this client (39); killing...\nfoo bar\nR5 no permissions (udev) usb:1\n";
        let d = parse_adb(banner);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].serial, "R5");
    }

    #[test]
    fn glob_filter() {
        assert!(glob("emulator-*", "emulator-5554"));
        assert!(!glob("emulator-*", "xemulator-5554"));
        assert!(glob("*-5554", "emulator-5554"));
        assert!(glob("a*c*e", "abcde"));
        assert!(glob("*", ""));
        assert!(!glob("abc", "abcd"));
        let cfg = PoolCfg { include: vec!["emulator-*".into()], exclude: vec!["emulator-5556".into()], statics: vec![] };
        assert!(allowed(&cfg, "emulator-5554"));
        assert!(!allowed(&cfg, "emulator-5556"));
        assert!(!allowed(&cfg, "R5CT"));
    }

    #[test]
    fn apply_online_offline_static_and_vanish() {
        let c = db::test_conn(&[]);
        let cfg = PoolCfg { include: vec![], exclude: vec!["0123456789".into()], statics: vec!["rig-a".into()] };
        apply(&c, 10, Some(&parse_adb(OUT)), &cfg).unwrap();
        let q = |s: &str| -> (String, String) {
            c.query_row("SELECT kind,state FROM devices WHERE serial=?", [s], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(q("emulator-5554"), ("android".into(), "online".into()));
        assert_eq!(q("R5CT1234ABC").1, "offline");
        assert_eq!(q("rig-a"), ("static".into(), "online".into()));
        assert!(c.query_row("SELECT 1 FROM devices WHERE serial='0123456789'", [], |_| Ok(())).is_err());
        apply(&c, 15, None, &cfg).unwrap(); // adb failed: nothing flips offline
        assert_eq!(q("emulator-5554").1, "online");
        let cfg2 = PoolCfg { include: vec![], exclude: vec!["emulator-5554".into()], statics: vec![] };
        apply(&c, 16, None, &cfg2).unwrap(); // adb failed, but config now disallows these
        assert_eq!(q("emulator-5554").1, "offline");
        assert_eq!(q("rig-a").1, "offline");
        apply(&c, 17, None, &cfg).unwrap();
        apply(&c, 20, Some(&[]), &cfg).unwrap(); // adb ok, device gone
        assert_eq!(q("emulator-5554").1, "offline");
        assert_eq!(q("rig-a").1, "online");
    }

    #[test]
    fn visible_and_filter() {
        let cfg =
            Config { pool: PoolCfg { include: vec![], exclude: vec!["x".into()], statics: vec!["rig".into()] }, ..Default::default() };
        assert!(visible(&cfg, "emulator-1") && visible(&cfg, "rig") && !visible(&cfg, "x"));
        let mut st = serde_json::json!({"devices":[{"serial":"x"},{"serial":"rig"}],"per_device":[{"serial":"x"}]});
        filter_state(&mut st, &cfg);
        assert_eq!(st["devices"].as_array().unwrap().len(), 1);
        assert_eq!(st["per_device"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn live_port_precedence() {
        assert_eq!(live_port(Some(1)).unwrap(), 1);
        let c: Config = toml::from_str("[live]\nport=9\n").unwrap();
        assert_eq!(c.live.port, Some(9));
        assert_eq!(Config::default().live.port, None);
    }

    #[test]
    fn config_defaults_and_parse() {
        let dir = std::env::temp_dir().join(format!("devq-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_config(&dir).unwrap().pool.include.is_empty());
        std::fs::write(dir.join("config.toml"), "[pool]\nexclude=[\"a\"]\nstatic=[\"rig\"]\n").unwrap();
        let c = load_config(&dir).unwrap();
        assert_eq!((c.pool.exclude, c.pool.statics), (vec!["a".to_string()], vec!["rig".to_string()]));
        let mut conn = db::open(&dir).unwrap();
        sync(&mut conn, 1, &Config::default()).unwrap();
        sync(&mut conn, 2, &Config::default()).unwrap(); // throttled
        std::fs::write(dir.join("config.toml"), "[pool]\nexcluded=[\"a\"]\n").unwrap();
        assert!(load_config(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
