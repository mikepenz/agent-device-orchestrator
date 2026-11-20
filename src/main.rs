mod db;
mod hook;
mod live;
mod pool;
mod service;
mod skills;
mod sys;

use clap::{Args, Parser, Subcommand};
use db::{now_ms, Res, Slot};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// Parse `90s`, `10m`, `1h30m`, `2d`, `500ms` (units: ms s m h d w; spaces allowed).
fn dur(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration '{s}' (use e.g. 90s, 10m, 1h30m)");
    let (mut total, mut rest) = (0u64, s.trim());
    if rest.is_empty() {
        return Err(bad());
    }
    while !rest.is_empty() {
        let n_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let n: u64 = rest[..n_end].parse().map_err(|_| bad())?;
        rest = &rest[n_end..];
        let u_end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        let ms = match &rest[..u_end] {
            "ms" => 1,
            "s" | "sec" | "secs" => 1_000,
            "m" | "min" | "mins" => 60_000,
            "h" | "hr" | "hrs" => 3_600_000,
            "d" => 86_400_000,
            "w" => 604_800_000,
            _ => return Err(bad()),
        };
        total = n.checked_mul(ms).and_then(|v| total.checked_add(v)).ok_or_else(bad)?;
        rest = rest[u_end..].trim_start();
    }
    Ok(Duration::from_millis(total))
}

#[derive(Parser)]
#[command(name = "devq", version, about = "Serialise access to Android devices between agents: queue, run, and watch device slots")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct SlotArgs {
    /// Who is asking (or set env DEVQ_AGENT)
    #[arg(long, env = "DEVQ_AGENT")]
    agent: Option<String>,
    /// Why the device is needed, shown in the dashboard
    #[arg(long)]
    purpose: Option<String>,
    /// Longer free-text description
    #[arg(long, default_value = "")]
    description: String,
    /// Expected hold time, 1s..1y
    #[arg(long, default_value = "10m", value_parser = dur)]
    expect: Duration,
    /// Hard limit; the slot is ended (and `run` kills the command) after this, 1s..1y, >= --expect
    #[arg(long, default_value = "30m", value_parser = dur)]
    max: Duration,
    /// Only accept this device serial (default: any free device)
    #[arg(long)]
    device: Option<String>,
}

const EXIT_CODES: &str = "Exit codes:
  0    granted (for `run`: the command succeeded)
  75   --wait expired; resume with `--ticket <id>`
  1    slot ended or unknown, or another error
  2    bad arguments
  130  interrupted (SIGINT)    143  terminated (SIGTERM)
  127  command could not be started
  else the child's own exit code (124 if --max killed it, 128+signal if signalled)";

#[derive(Subcommand)]
enum Cmd {
    /// Request a device slot; blocks until granted. Prints `<slot> <serial>`
    #[command(after_help = EXIT_CODES, mut_arg("agent", |a| a.required_unless_present("ticket")), mut_arg("purpose", |a| a.required_unless_present("ticket")))]
    Request {
        #[command(flatten)]
        slot: SlotArgs,
        /// Give up waiting after this long (exit 75, prints `ticket <id>`); default: wait forever
        #[arg(long, value_parser = dur)]
        wait: Option<Duration>,
        /// Resume a ticket printed by an expired --wait (--agent/--purpose not needed)
        #[arg(long)]
        ticket: Option<i64>,
        /// Print `{"slot":id,"device":serial}` instead of `<slot> <serial>`
        #[arg(long)]
        json: bool,
    },
    /// Release (or cancel) a slot by id
    Release {
        /// Slot id printed by `request`
        id: i64,
    },
    /// Acquire a device, run a command with ANDROID_SERIAL set, then release it.
    /// The command runs in its own process group with stdin from /dev/null.
    #[command(after_help = EXIT_CODES, mut_arg("agent", |a| a.required(true)), mut_arg("purpose", |a| a.required(true)))]
    Run {
        #[command(flatten)]
        slot: SlotArgs,
        /// Command and arguments, after `--`
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Show devices, who holds them, and the queue
    Status {
        /// Print the full /api/state JSON document
        #[arg(long)]
        json: bool,
    },
    /// List known devices (from adb and config)
    Devices {
        /// Print a JSON array
        #[arg(long)]
        json: bool,
    },
    /// Serve the live dashboard on 127.0.0.1
    Live {
        /// Port to listen on
        #[arg(long, env = "DEVQ_PORT")]
        port: Option<u16>,
        /// Open the dashboard in the default browser
        #[arg(long)]
        open: bool,
    },
    /// Run `devq live` as a macOS launchd service (starts at login, restarts on crash)
    Service {
        #[command(subcommand)]
        cmd: service::ServiceCmd,
    },
    /// Install and manage the bundled agent skill (Claude Code `SKILL.md`)
    Skills {
        #[command(subcommand)]
        cmd: skills::SkillsCmd,
    },
    /// Optional guardrail: block raw adb/connected-test commands that have no devq slot.
    /// Without a subcommand it reads a PreToolUse/BeforeTool JSON on stdin (exit 2 = blocked)
    Hook {
        /// Fail closed (exit 2) on internal errors instead of allowing
        #[arg(long)]
        strict: bool,
        #[command(subcommand)]
        cmd: Option<hook::HookCmd>,
    },
}

enum Acquired {
    Granted(Box<Slot>),
    Expired(i64),
}

/// SIGINT/SIGTERM flags, registered once so no signal is lost between phases.
struct Sigs {
    int: Arc<AtomicBool>,
    term: Arc<AtomicBool>,
}

impl Sigs {
    fn new() -> Sigs {
        let (int, term) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        sys::install_signals(&int, &term);
        Sigs { int, term }
    }
}

fn ceil_secs(d: Duration) -> u64 {
    d.as_secs() + (d.subsec_nanos() > 0) as u64
}

/// Enqueue (or resume `ticket`) and poll until granted, `wait` expires, or a signal cancels.
/// `holder` = Some(command) for `devq run`: records our pid at enqueue so orphan detection covers waiting.
fn acquire(a: &SlotArgs, wait: Option<Duration>, ticket: Option<i64>, holder: Option<String>, sig: &Sigs) -> Res<Acquired> {
    let (expect, max) = (ceil_secs(a.expect), ceil_secs(a.max));
    const YEAR: u64 = 365 * 86_400;
    if expect < 1 || max < 1 || max < expect || max > YEAR {
        eprintln!("devq: --expect/--max must be >= 1s, <= 1 year, and --max >= --expect");
        std::process::exit(2);
    }
    let home = pool::devq_home();
    let cfg = pool::load_config(&home)?;
    let mut conn = db::open(&home)?;
    let id = match ticket {
        Some(t) => t,
        None => {
            let ns = db::NewSlot {
                agent: a.agent.clone().unwrap_or_default(),
                purpose: a.purpose.clone().unwrap_or_default(),
                description: a.description.clone(),
                want_device: a.device.clone(),
                expect_secs: expect as i64,
                max_secs: max as i64,
                holder_pid: holder.as_ref().map(|_| std::process::id() as i64),
                command: holder,
            };
            db::immediate(&mut conn, |c| db::enqueue(c, &ns, now_ms()))?
        }
    };
    let start = std::time::Instant::now();
    loop {
        let now = now_ms();
        pool::sync(&mut conn, now, &cfg)?;
        let s = db::immediate(&mut conn, |c| {
            db::heartbeat(c, id, now)?;
            db::tick(c, now)?;
            db::get_slot(c, id)
        })?;
        let Some(s) = s else { return Err(format!("unknown ticket {id}").into()) };
        match s.state.as_str() {
            "active" => return Ok(Acquired::Granted(Box::new(s))),
            "done" => return Err(format!("slot {id} ended ({})", s.end_reason.unwrap_or_default()).into()),
            _ => {}
        }
        let (i, t) = (sig.int.load(Relaxed), sig.term.load(Relaxed));
        if i || t {
            let _ = db::immediate(&mut conn, |c| db::release(c, id, now_ms()));
            eprintln!("devq: cancelled waiting slot {id}");
            std::process::exit(if t { 143 } else { 130 });
        }
        if wait.is_some_and(|w| start.elapsed() >= w) {
            return Ok(Acquired::Expired(id));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn run(a: SlotArgs, cmd: Vec<String>, sig: &Sigs) -> Res<i32> {
    let Acquired::Granted(slot) = acquire(&a, None, None, Some(cmd.join(" ")), sig)? else { unreachable!() };
    let mut conn = db::open(&pool::devq_home())?;
    let mut pgid = None;
    let r = run_child(&mut conn, &slot, &cmd, sig, &mut pgid);
    if r.is_err() {
        // any failure after the grant: no stray child, no leaked slot
        if let Some(g) = pgid {
            sys::kill_tree(g, true);
        }
        let _ = db::immediate(&mut conn, |c| db::release(c, slot.id, now_ms()));
    }
    r
}

fn run_child(conn: &mut rusqlite::Connection, slot: &Slot, cmd: &[String], sig: &Sigs, pgid: &mut Option<i64>) -> Res<i32> {
    let serial = slot.device.clone().unwrap_or_default();
    // Own process group so a terminal Ctrl-C reaches only devq, which forwards it to the group.
    // That makes the group a background one, so stdin is /dev/null (a tty read would SIGTTIN).
    let mut command = std::process::Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .env("ANDROID_SERIAL", &serial)
        .env("DEVQ_DEVICE", &serial)
        .env("DEVQ_SLOT", slot.id.to_string())
        .stdin(std::process::Stdio::null());
    sys::prepare_grouped(&mut command);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("devq: cannot run {}: {e}", cmd[0]);
            let _ = db::immediate(conn, |c| db::release(c, slot.id, now_ms()));
            return Ok(127);
        }
    };
    let g = child.id() as i64;
    *pgid = Some(g);
    db::immediate(conn, |c| db::set_child_pgid(c, slot.id, g))?;
    let deadline = slot.deadline.unwrap_or(i64::MAX);
    let (mut sent_int, mut sent_term, mut timed_out) = (false, false, false);
    let mut kill_at = None;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if sig.int.load(Relaxed) && !sent_int {
            sent_int = true;
            sys::interrupt(g);
        }
        if sig.term.load(Relaxed) && !sent_term {
            sent_term = true;
            sys::kill_tree(g, false);
        }
        if now_ms() > deadline && !timed_out {
            timed_out = true;
            eprintln!("devq: --max elapsed, terminating command");
            sys::kill_tree(g, false);
            kill_at = Some(std::time::Instant::now() + Duration::from_secs(5));
        }
        if kill_at.is_some_and(|t| std::time::Instant::now() >= t) {
            kill_at = None;
            sys::kill_tree(g, true);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // Leader is gone; make sure no grandchildren still use the device before releasing it.
    if sys::kill_tree(g, false) {
        let t = std::time::Instant::now();
        while sys::group_exists(g) && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(50));
        }
        sys::kill_tree(g, true);
    }
    let code = sys::exit_code(&status);
    let code = if timed_out && code == 0 { 124 } else { code };
    db::immediate(conn, |c| {
        db::set_exit_code(c, slot.id, code as i64)?;
        // "already done" is fine: another devq's tick may have reaped the slot first.
        let _ = if timed_out { db::end_slot(c, slot.id, now_ms(), "timeout") } else { db::release(c, slot.id, now_ms()) };
        Ok(())
    })?;
    Ok(code)
}

fn fmt_dur(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

fn status(json: bool) -> Res<()> {
    let home = pool::devq_home();
    let cfg = pool::load_config(&home)?;
    let mut conn = db::open(&home)?;
    let now = now_ms();
    pool::sync(&mut conn, now, &cfg)?;
    db::immediate(&mut conn, |c| db::tick(c, now))?;
    let mut st = db::state_json(&conn, now, 3600)?;
    pool::filter_state(&mut st, &cfg);
    if json {
        println!("{st}");
        return Ok(());
    }
    for d in st["devices"].as_array().unwrap() {
        let (serial, state) = (d["serial"].as_str().unwrap(), d["state"].as_str().unwrap());
        if d["slot"].is_null() {
            println!("{serial:<20} {state:<8} free");
        } else {
            let s = &d["slot"];
            let el = now - s["granted_at"].as_i64().unwrap_or(now);
            println!(
                "{serial:<20} {state:<8} #{} {} \"{}\" elapsed {} / expect {}s",
                s["id"],
                s["agent"].as_str().unwrap(),
                s["purpose"].as_str().unwrap(),
                fmt_dur(el),
                s["expect_secs"]
            );
        }
    }
    let q = st["queue"].as_array().unwrap();
    println!("queue: {}", if q.is_empty() { "empty" } else { "" });
    for (i, s) in q.iter().enumerate() {
        println!(
            "  {}. #{} {} \"{}\" waiting {}",
            i + 1,
            s["id"],
            s["agent"].as_str().unwrap(),
            s["purpose"].as_str().unwrap(),
            fmt_dur(now - s["requested_at"].as_i64().unwrap())
        );
    }
    Ok(())
}

fn devices(json: bool) -> Res<()> {
    let home = pool::devq_home();
    let cfg = pool::load_config(&home)?;
    let mut conn = db::open(&home)?;
    pool::sync(&mut conn, now_ms(), &cfg)?;
    let mut st = conn.prepare("SELECT serial,kind,model,state FROM devices ORDER BY serial")?;
    let rows: Vec<(String, String, String, String)> = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|r| pool::visible(&cfg, &r.0))
        .collect();
    if json {
        let v: Vec<_> = rows.iter().map(|(s, k, m, st)| serde_json::json!({"serial": s, "kind": k, "model": m, "state": st})).collect();
        println!("{}", serde_json::Value::Array(v));
    } else {
        println!("{:<20} {:<8} {:<8} MODEL", "SERIAL", "KIND", "STATE");
        for (s, k, m, st) in rows {
            println!("{s:<20} {k:<8} {st:<8} {m}");
        }
    }
    Ok(())
}

fn go(cli: Cli) -> Res<i32> {
    match cli.cmd {
        Cmd::Request { slot, wait, ticket, json } => match acquire(&slot, wait, ticket, None, &Sigs::new())? {
            Acquired::Granted(s) => {
                let dev = s.device.unwrap_or_default();
                if json {
                    println!("{}", serde_json::json!({"slot": s.id, "device": dev}));
                } else {
                    println!("{} {}", s.id, dev);
                }
                Ok(0)
            }
            Acquired::Expired(id) => {
                println!("ticket {id}");
                Ok(75)
            }
        },
        Cmd::Release { id } => {
            let mut conn = db::open(&pool::devq_home())?;
            db::immediate(&mut conn, |c| db::release(c, id, now_ms()))?;
            Ok(0)
        }
        Cmd::Run { slot, cmd } => run(slot, cmd, &Sigs::new()),
        Cmd::Status { json } => status(json).map(|_| 0),
        Cmd::Devices { json } => devices(json).map(|_| 0),
        Cmd::Live { port, open } => live::serve(pool::live_port(port)?, open).map(|_| 0),
        Cmd::Service { cmd } => service::run(cmd),
        Cmd::Hook { strict, cmd } => hook::run(strict, cmd, sys::home_dir().ok().as_deref(), &std::env::current_dir()?),
        Cmd::Skills { cmd } => {
            let home = sys::home_dir().ok();
            skills::run(cmd, home.as_deref(), &std::env::current_dir()?)
        }
    }
}

fn main() {
    match go(Cli::parse()) {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            eprintln!("devq: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod dur_tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        assert_eq!(dur("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(dur("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(dur("1h 30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(dur("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(dur("2d").unwrap(), Duration::from_secs(172_800));
        for bad in ["", "10", "m", "10x", "-5s", "99999999999999999999w"] {
            assert!(dur(bad).is_err(), "{bad} should fail");
        }
        assert_eq!(fmt_dur(42_900), "42s");
        assert_eq!(fmt_dur(125_000), "2m 5s");
        assert_eq!(fmt_dur(3_725_000), "1h 2m 5s");
    }
}
