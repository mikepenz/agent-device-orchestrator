use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

const ABANDON_MS: i64 = 30_000;
const GRACE_MS: i64 = 5_000;
/// Only waiters that polled this recently are granted (a dead ticket must not hold a device).
const LIVE_MS: i64 = 8_000; // > 5s adb timeout + margin

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
  serial TEXT PRIMARY KEY, kind TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS slots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent TEXT NOT NULL, purpose TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  want_device TEXT, expect_secs INTEGER NOT NULL, max_secs INTEGER NOT NULL,
  state TEXT NOT NULL, device TEXT,
  requested_at INTEGER NOT NULL, granted_at INTEGER, released_at INTEGER, deadline INTEGER,
  heartbeat_at INTEGER NOT NULL, holder_pid INTEGER, command TEXT, exit_code INTEGER,
  end_reason TEXT, child_pgid INTEGER);
CREATE INDEX IF NOT EXISTS slots_state ON slots(state);
CREATE TABLE IF NOT EXISTS blocked (at INTEGER NOT NULL, agent TEXT NOT NULL, command TEXT NOT NULL, reason TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL);
";

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Opens `$home/devq.db`, creating dir, schema, WAL and a 5s busy timeout.
pub fn open(home: &Path) -> Res<Connection> {
    std::fs::create_dir_all(home)?;
    let path = home.join("devq.db");
    let mut conn = Connection::open(&path)?;
    // Fast path: already set up, so read-only commands don't queue behind writers.
    conn.busy_timeout(Duration::from_secs(5))?;
    if is_ready(&conn) {
        return Ok(conn);
    }
    // On a fresh file, WAL switch and DDL can return SQLITE_BUSY without consulting the busy handler
    // (deadlock avoidance), so retry. Short busy_timeout per attempt keeps the total near 5s.
    conn.busy_timeout(Duration::from_millis(100))?;
    let start = std::time::Instant::now();
    let mode = loop {
        match init(&mut conn) {
            Err(e) if is_busy(&e) && start.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(50)),
            r => break r?,
        }
    };
    if mode != "wal" {
        return Err(format!("devq needs a local filesystem that supports SQLite WAL: {}", path.display()).into());
    }
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

fn is_ready(conn: &Connection) -> bool {
    let wal = conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).is_ok_and(|m| m == "wal");
    wal && conn
        .query_row("SELECT COUNT(*) FROM pragma_table_info('slots') WHERE name='child_pgid'", [], |r| r.get::<_, i64>(0))
        .is_ok_and(|n| n == 1)
        && conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='blocked'", [], |r| r.get::<_, i64>(0)).is_ok_and(|n| n == 1)
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

/// Returns the resulting journal mode.
fn init(conn: &mut Connection) -> Result<String, rusqlite::Error> {
    let mode = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
    let t = Transaction::new(conn, TransactionBehavior::Immediate)?;
    t.execute_batch(SCHEMA)?;
    let has: i64 = t.query_row("SELECT COUNT(*) FROM pragma_table_info('slots') WHERE name='child_pgid'", [], |r| r.get(0))?;
    if has == 0 {
        // pre-existing dbs; tolerate a concurrent upgrader ("duplicate column")
        if let Err(e) = t.execute("ALTER TABLE slots ADD COLUMN child_pgid INTEGER", []) {
            if !e.to_string().contains("duplicate column") {
                return Err(e);
            }
        }
    }
    t.commit()?;
    Ok(mode)
}

/// Runs `f` inside BEGIN IMMEDIATE and commits. Use around `tick` + any writes.
pub fn immediate<T>(conn: &mut Connection, f: impl FnOnce(&Connection) -> Res<T>) -> Res<T> {
    let t = Transaction::new(conn, TransactionBehavior::Immediate)?;
    let r = f(&t)?;
    t.commit()?;
    Ok(r)
}

pub struct NewSlot {
    pub agent: String,
    pub purpose: String,
    pub description: String,
    pub want_device: Option<String>,
    pub expect_secs: i64,
    pub max_secs: i64,
    /// Set at enqueue by `devq run` so orphan detection covers waiting slots too.
    pub holder_pid: Option<i64>,
    pub command: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub id: i64,
    pub agent: String,
    pub purpose: String,
    pub description: String,
    pub want_device: Option<String>,
    pub expect_secs: i64,
    pub max_secs: i64,
    pub state: String,
    pub device: Option<String>,
    pub requested_at: i64,
    pub granted_at: Option<i64>,
    pub released_at: Option<i64>,
    pub deadline: Option<i64>,
    #[allow(dead_code)] // only the DB reads it
    pub heartbeat_at: i64,
    pub holder_pid: Option<i64>,
    pub command: Option<String>,
    pub exit_code: Option<i64>,
    pub end_reason: Option<String>,
    pub child_pgid: Option<i64>,
}

const COLS: &str = "id,agent,purpose,description,want_device,expect_secs,max_secs,state,device,\
requested_at,granted_at,released_at,deadline,heartbeat_at,holder_pid,command,exit_code,end_reason,child_pgid";

fn slot_from(r: &rusqlite::Row) -> rusqlite::Result<Slot> {
    Ok(Slot {
        id: r.get(0)?,
        agent: r.get(1)?,
        purpose: r.get(2)?,
        description: r.get(3)?,
        want_device: r.get(4)?,
        expect_secs: r.get(5)?,
        max_secs: r.get(6)?,
        state: r.get(7)?,
        device: r.get(8)?,
        requested_at: r.get(9)?,
        granted_at: r.get(10)?,
        released_at: r.get(11)?,
        deadline: r.get(12)?,
        heartbeat_at: r.get(13)?,
        holder_pid: r.get(14)?,
        command: r.get(15)?,
        exit_code: r.get(16)?,
        end_reason: r.get(17)?,
        child_pgid: r.get(18)?,
    })
}

fn slots_where(conn: &Connection, clause: &str, p: impl rusqlite::Params) -> Res<Vec<Slot>> {
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM slots WHERE {clause}"))?;
    let rows = st.query_map(p, slot_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn enqueue(conn: &Connection, s: &NewSlot, now: i64) -> Res<i64> {
    conn.execute(
        "INSERT INTO slots(agent,purpose,description,want_device,expect_secs,max_secs,state,requested_at,heartbeat_at,holder_pid,command)
         VALUES(?,?,?,?,?,?,'waiting',?,?,?,?)",
        params![s.agent, s.purpose, s.description, s.want_device, s.expect_secs, s.max_secs, now, now, s.holder_pid, s.command],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn heartbeat(conn: &Connection, id: i64, now: i64) -> Res<()> {
    conn.execute("UPDATE slots SET heartbeat_at=? WHERE id=? AND state='waiting'", params![now, id])?;
    Ok(())
}

pub fn get_slot(conn: &Connection, id: i64) -> Res<Option<Slot>> {
    Ok(slots_where(conn, "id=?", [id])?.pop())
}

/// Releases an active slot (`released`) or cancels a waiting one (`cancelled`).
/// Errors if the id is unknown or already done.
pub fn release(conn: &Connection, id: i64, now: i64) -> Res<()> {
    let n = conn.execute(
        "UPDATE slots SET released_at=?1, state='done',
           end_reason=CASE state WHEN 'active' THEN 'released' ELSE 'cancelled' END
         WHERE id=?2 AND state!='done'",
        params![now, id],
    )?;
    if n == 0 {
        return Err(format!("slot {id} unknown or already done").into());
    }
    Ok(())
}

#[cfg(test)]
pub fn set_holder(conn: &Connection, id: i64, pid: i64, command: &str) -> Res<()> {
    conn.execute("UPDATE slots SET holder_pid=?, command=? WHERE id=?", params![pid, command, id])?;
    Ok(())
}

pub fn set_exit_code(conn: &Connection, id: i64, code: i64) -> Res<()> {
    conn.execute("UPDATE slots SET exit_code=? WHERE id=?", params![code, id])?;
    Ok(())
}

fn pid_dead(pid: i64) -> bool {
    !crate::sys::pid_alive(pid)
}

pub fn set_child_pgid(conn: &Connection, id: i64, pgid: i64) -> Res<()> {
    conn.execute("UPDATE slots SET child_pgid=? WHERE id=?", params![pgid, id])?;
    Ok(())
}

/// Ends an active/waiting slot with `reason`. Errors if already done.
pub fn end_slot(conn: &Connection, id: i64, now: i64, reason: &str) -> Res<()> {
    let n =
        conn.execute("UPDATE slots SET state='done', released_at=?, end_reason=? WHERE id=? AND state!='done'", params![now, reason, id])?;
    if n == 0 {
        return Err(format!("slot {id} unknown or already done").into());
    }
    Ok(())
}

/// Reaping (timeout/orphaned) must not leave the holder's command running on a device we re-grant.
fn reap(conn: &Connection, s: &Slot, now: i64, reason: &str) -> Res<()> {
    if let Some(g) = s.child_pgid {
        crate::sys::kill_tree(g, true);
    }
    end(conn, s.id, now, reason)
}

fn end(conn: &Connection, id: i64, now: i64, reason: &str) -> Res<()> {
    conn.execute("UPDATE slots SET state='done', released_at=?, end_reason=? WHERE id=?", params![now, reason, id])?;
    Ok(())
}

/// Reap then allocate. Call inside `immediate`.
pub fn tick(conn: &Connection, now: i64) -> Res<()> {
    // A running `devq run` child gets a 5s SIGTERM grace after the deadline before the hard reap.
    let q = "state='active' AND ((child_pgid IS NULL AND deadline<?1) OR deadline+?2<?1)";
    for s in slots_where(conn, q, params![now, GRACE_MS])? {
        reap(conn, &s, now, "timeout")?;
    }
    for s in slots_where(conn, "state IN ('active','waiting') AND holder_pid IS NOT NULL", [])? {
        if pid_dead(s.holder_pid.unwrap()) {
            reap(conn, &s, now, "orphaned")?;
        }
    }
    for s in slots_where(conn, "state='waiting' AND heartbeat_at<?", [now - ABANDON_MS])? {
        end(conn, s.id, now, "abandoned")?;
    }

    let mut free: Vec<String> = {
        let mut st = conn.prepare(
            "SELECT serial FROM devices WHERE state='online'
             AND serial NOT IN (SELECT device FROM slots WHERE state='active') ORDER BY serial",
        )?;
        let v = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        v
    };
    for s in slots_where(conn, "state='waiting' AND heartbeat_at>=? ORDER BY id", [now - LIVE_MS])? {
        let pos = match &s.want_device {
            Some(w) => free.iter().position(|d| d == w),
            None => (!free.is_empty()).then_some(0),
        };
        if let Some(i) = pos {
            let dev = free.remove(i);
            conn.execute(
                "UPDATE slots SET state='active', device=?1, granted_at=?2, heartbeat_at=?2, deadline=?3 WHERE id=?4",
                params![dev, now, now + s.max_secs * 1000, s.id],
            )?;
        }
    }
    Ok(())
}

/// Nearest-rank percentile of a sorted slice.
fn pct(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = (p * sorted.len() as f64).ceil() as usize;
    sorted[i.clamp(1, sorted.len()) - 1]
}

fn mean(v: &[i64]) -> i64 {
    if v.is_empty() {
        0
    } else {
        v.iter().sum::<i64>() / v.len() as i64
    }
}

/// Builds the `/api/state` document.
pub fn state_json(conn: &Connection, now: i64, window_secs: i64) -> Res<Value> {
    let window_secs = window_secs.clamp(1, 31_536_000);
    let window_ms = window_secs * 1000;
    let since = now - window_ms;
    let active = slots_where(conn, "state='active'", [])?;
    let queue = slots_where(conn, "state='waiting' ORDER BY id", [])?;
    let overlap = slots_where(conn, "granted_at IS NOT NULL AND (released_at IS NULL OR released_at>=?) ORDER BY id DESC", [since])?;
    let hist = slots_where(conn, "granted_at IS NOT NULL AND (granted_at>=? OR state='active') ORDER BY id DESC", [since])?;

    let mut devices = vec![];
    let mut serials = vec![];
    let mut st = conn.prepare("SELECT serial,kind,model,state FROM devices ORDER BY serial")?;
    let rows =
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    for r in rows {
        let (serial, kind, model, state) = r?;
        let slot = active.iter().find(|s| s.device.as_deref() == Some(&serial)).map(|s| {
            json!({"id": s.id, "agent": s.agent, "purpose": s.purpose, "description": s.description,
                   "command": s.command, "granted_at": s.granted_at, "expect_secs": s.expect_secs,
                   "max_secs": s.max_secs, "deadline": s.deadline})
        });
        devices.push(json!({"serial": serial, "kind": kind, "model": model, "state": state, "slot": slot}));
        serials.push(serial);
    }

    let busy = |s: &Slot| -> i64 {
        let end = s.released_at.unwrap_or(now).min(now);
        (end - s.granted_at.unwrap().max(since)).clamp(0, window_ms)
    };
    let waits = |v: &[&Slot]| -> Vec<i64> { v.iter().map(|s| s.granted_at.unwrap() - s.requested_at).collect() };

    let all: Vec<&Slot> = hist.iter().collect();
    let mut w = waits(&all);
    w.sort();
    let done: Vec<&Slot> = all.iter().copied().filter(|s| s.state == "done").collect();
    let holds: Vec<i64> = done.iter().map(|s| s.released_at.unwrap() - s.granted_at.unwrap()).collect();
    let expect_total: i64 = done.iter().map(|s| s.expect_secs * 1000).sum();
    let reason = |r: &str| all.iter().filter(|s| s.end_reason.as_deref() == Some(r)).count();
    let abandoned: i64 =
        conn.query_row("SELECT COUNT(*) FROM slots WHERE end_reason='abandoned' AND requested_at>=?", [since], |r| r.get(0))?;
    let stats = json!({
        "grants": all.len(), "wait_p50_ms": pct(&w, 0.5), "wait_p95_ms": pct(&w, 0.95),
        "hold_mean_ms": mean(&holds),
        "expect_ratio": if expect_total > 0 { holds.iter().sum::<i64>() as f64 / expect_total as f64 } else { 0.0 },
        "timeouts": reason("timeout"), "orphaned": reason("orphaned"), "abandoned": abandoned,
        "queued": queue.len(),
    });

    let per_device: Vec<Value> = serials
        .iter()
        .map(|d| {
            let v: Vec<&Slot> = all.iter().copied().filter(|s| s.device.as_deref() == Some(d)).collect();
            let b: i64 = overlap.iter().filter(|s| s.device.as_deref() == Some(d)).map(busy).sum::<i64>().min(window_ms);
            json!({"serial": d, "grants": v.len(), "busy_ms": b, "utilisation": b as f64 / window_ms as f64})
        })
        .collect();

    let mut agents: Vec<&str> = all.iter().map(|s| s.agent.as_str()).collect();
    agents.sort();
    agents.dedup();
    let per_agent: Vec<Value> = agents
        .iter()
        .map(|a| {
            let v: Vec<&Slot> = all.iter().copied().filter(|s| s.agent == *a).collect();
            json!({"agent": a, "grants": v.len(), "busy_ms": overlap.iter().filter(|s| s.agent == *a).map(busy).sum::<i64>(),
               "wait_mean_ms": mean(&waits(&v)),
               "timeouts": v.iter().filter(|s| s.end_reason.as_deref() == Some("timeout")).count()})
        })
        .collect();

    let history: Vec<Value> = overlap
        .iter()
        .map(|s| {
            json!({"id": s.id, "device": s.device, "agent": s.agent, "purpose": s.purpose,
               "description": s.description, "command": s.command, "want_device": s.want_device,
               "requested_at": s.requested_at, "granted_at": s.granted_at, "released_at": s.released_at,
               "end_reason": s.end_reason, "exit_code": s.exit_code, "expect_secs": s.expect_secs,
               "max_secs": s.max_secs})
        })
        .collect();
    let queue: Vec<Value> = queue
        .iter()
        .map(|s| {
            json!({"id": s.id, "agent": s.agent, "purpose": s.purpose, "description": s.description,
               "want_device": s.want_device, "requested_at": s.requested_at, "expect_secs": s.expect_secs})
        })
        .collect();

    let mut bl = conn.prepare("SELECT at,agent,command,reason FROM blocked WHERE at>=? ORDER BY at DESC LIMIT 50")?;
    let blocked: Vec<Value> = bl
        .query_map([since], |r| Ok(json!({"at": r.get::<_, i64>(0)?, "agent": r.get::<_, String>(1)?, "command": r.get::<_, String>(2)?, "reason": r.get::<_, String>(3)?})))?
        .collect::<Result<_, _>>()?;

    Ok(json!({"now": now, "since": since, "devices": devices, "queue": queue, "history": history,
              "stats": stats, "per_device": per_device, "per_agent": per_agent, "blocked": blocked}))
}

#[cfg(test)]
pub fn test_conn(serials: &[&str]) -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(SCHEMA).unwrap();
    for s in serials {
        c.execute("INSERT INTO devices VALUES(?,'static','m','online',0,0)", [s]).unwrap();
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(agent: &str, want: Option<&str>) -> NewSlot {
        NewSlot {
            agent: agent.into(),
            purpose: "p".into(),
            description: String::new(),
            want_device: want.map(String::from),
            expect_secs: 10,
            max_secs: 60,
            holder_pid: None,
            command: None,
        }
    }
    fn st(c: &Connection, id: i64) -> Slot {
        get_slot(c, id).unwrap().unwrap()
    }

    #[test]
    fn fifo_order() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 1).unwrap();
        immediate(&mut c, |c| tick(c, 2)).unwrap();
        assert_eq!(st(&c, a).state, "active");
        assert_eq!(st(&c, b).state, "waiting");
        release(&c, a, 3).unwrap();
        immediate(&mut c, |c| tick(c, 4)).unwrap();
        assert_eq!(st(&c, b).device.as_deref(), Some("d1"));
    }

    #[test]
    fn pinned_does_not_block() {
        let mut c = test_conn(&["d1", "d2"]);
        let a = enqueue(&c, &ns("a", Some("d1")), 0).unwrap();
        let b = enqueue(&c, &ns("b", Some("d1")), 1).unwrap(); // pinned to busy d1
        let x = enqueue(&c, &ns("x", None), 2).unwrap();
        immediate(&mut c, |c| tick(c, 3)).unwrap();
        assert_eq!(st(&c, a).state, "active");
        assert_eq!(st(&c, b).state, "waiting");
        assert_eq!(st(&c, x).device.as_deref(), Some("d2"));
    }

    #[test]
    fn pinned_to_offline_is_skipped() {
        let mut c = test_conn(&["d1"]);
        c.execute("INSERT INTO devices VALUES('off','android','','offline',0,0)", []).unwrap();
        let a = enqueue(&c, &ns("a", Some("off")), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 1).unwrap();
        immediate(&mut c, |c| tick(c, 2)).unwrap();
        assert_eq!(st(&c, a).state, "waiting");
        assert_eq!(st(&c, b).state, "active");
    }

    #[test]
    fn timeout_reap() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 0).unwrap();
        immediate(&mut c, |c| tick(c, 0)).unwrap(); // deadline 60_000
        immediate(&mut c, |c| {
            heartbeat(c, b, 59_000)?;
            tick(c, 60_001)
        })
        .unwrap();
        assert_eq!(st(&c, a).end_reason.as_deref(), Some("timeout"));
        assert_eq!(st(&c, b).state, "active");
    }

    #[test]
    fn abandoned_reap() {
        let mut c = test_conn(&[]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 0).unwrap();
        heartbeat(&c, b, 20_000).unwrap();
        immediate(&mut c, |c| tick(c, 31_000)).unwrap();
        assert_eq!(st(&c, a).end_reason.as_deref(), Some("abandoned"));
        assert_eq!(st(&c, b).state, "waiting");
    }

    #[cfg(unix)]
    #[test]
    fn orphan_reap_dead_pid() {
        let mut c = test_conn(&["d1"]);
        let mut ch = std::process::Command::new("true").spawn().unwrap();
        let dead = ch.id() as i64;
        ch.wait().unwrap();
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 0).unwrap();
        immediate(&mut c, |c| tick(c, 1)).unwrap();
        set_holder(&c, a, dead, "true").unwrap();
        immediate(&mut c, |c| tick(c, 2)).unwrap();
        assert_eq!(st(&c, a).end_reason.as_deref(), Some("orphaned"));
        assert_eq!(st(&c, b).state, "active");
        // live pid (ours) is not reaped
        set_holder(&c, b, std::process::id() as i64, "x").unwrap();
        immediate(&mut c, |c| tick(c, 3)).unwrap();
        assert_eq!(st(&c, b).state, "active");
    }

    #[test]
    fn release_and_cancel() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 0).unwrap();
        immediate(&mut c, |c| tick(c, 1)).unwrap();
        release(&c, a, 5).unwrap();
        release(&c, b, 5).unwrap();
        assert_eq!(st(&c, a).end_reason.as_deref(), Some("released"));
        assert_eq!(st(&c, b).end_reason.as_deref(), Some("cancelled"));
        assert!(release(&c, a, 6).is_err());
        assert!(release(&c, 999, 6).is_err());
        set_exit_code(&c, a, 3).unwrap();
        assert_eq!(st(&c, a).exit_code, Some(3));
    }

    #[test]
    fn end_slot_reason_clamp_util_and_pgkill() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        immediate(&mut c, |c| tick(c, 1)).unwrap();
        // child in its own group, registered on the slot; timeout reap must kill it
        #[cfg(unix)]
        let mut ch = {
            use std::os::unix::process::CommandExt;
            let ch = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
            set_child_pgid(&c, a, ch.id() as i64).unwrap();
            ch
        };
        immediate(&mut c, |c| tick(c, 65_002)).unwrap();
        assert_eq!(st(&c, a).end_reason.as_deref(), Some("timeout"));
        #[cfg(unix)]
        assert!(!ch.wait().unwrap().success());
        assert!(end_slot(&c, a, 62_000, "timeout").is_err());
        // window clamps instead of panicking / NaN
        let j = state_json(&c, 100_000, -5).unwrap();
        assert_eq!(j["since"], 99_000);
        assert!(j["per_device"][0]["utilisation"].as_f64().unwrap().is_finite());
        // done slot granted before `since` but released inside it counts toward busy, not grants
        let j = state_json(&c, 100_000, 50).unwrap(); // since = 50_000; slot ran 1..65_002
        assert_eq!(j["stats"]["grants"], 0);
        assert_eq!(j["history"].as_array().unwrap().len(), 1); // overlapping slot still listed
        assert_eq!(j["per_device"][0]["busy_ms"], 15_002);
    }

    #[cfg(unix)]
    #[test]
    fn stale_waiter_not_granted_and_waiting_orphan() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap();
        let b = enqueue(&c, &ns("b", None), 9_000).unwrap();
        immediate(&mut c, |c| tick(c, 10_000)).unwrap(); // a's last poll was 10s ago
        assert_eq!(st(&c, a).state, "waiting");
        assert_eq!(st(&c, b).state, "active");
        let mut ch = std::process::Command::new("true").spawn().unwrap();
        let dead = ch.id() as i64;
        ch.wait().unwrap();
        let w = enqueue(&c, &NewSlot { holder_pid: Some(dead), ..ns("w", None) }, 10_000).unwrap();
        immediate(&mut c, |c| tick(c, 10_001)).unwrap();
        assert_eq!(st(&c, w).end_reason.as_deref(), Some("orphaned"));
    }

    #[test]
    fn child_gets_grace_before_hard_reap() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 0).unwrap(); // deadline 60_000
        immediate(&mut c, |c| tick(c, 1)).unwrap();
        set_child_pgid(&c, a, 1_000_000_000).unwrap(); // nonexistent group: kill is a no-op
        immediate(&mut c, |c| tick(c, 62_000)).unwrap();
        assert_eq!(st(&c, a).state, "active");
        immediate(&mut c, |c| tick(c, 65_002)).unwrap();
        assert_eq!(st(&c, a).end_reason.as_deref(), Some("timeout"));
    }

    #[test]
    fn state_json_shape() {
        let mut c = test_conn(&["d1"]);
        let a = enqueue(&c, &ns("a", None), 1_000).unwrap();
        let _b = enqueue(&c, &ns("b", None), 1_500).unwrap();
        immediate(&mut c, |c| tick(c, 2_000)).unwrap();
        set_holder(&c, a, std::process::id() as i64, "cmd").unwrap();
        let j = state_json(&c, 12_000, 100).unwrap();
        assert_eq!(j["now"], 12_000);
        assert_eq!(j["since"], 12_000 - 100_000);
        assert_eq!(j["devices"][0]["slot"]["command"], "cmd");
        assert_eq!(j["devices"][0]["slot"]["deadline"], 62_000);
        assert_eq!(j["queue"][0]["agent"], "b");
        assert_eq!(j["history"][0]["end_reason"], Value::Null);
        assert_eq!(j["history"][0]["command"], "cmd");
        assert_eq!(j["history"][0]["description"], "");
        assert_eq!(j["history"][0]["want_device"], Value::Null);
        assert_eq!(j["history"][0]["max_secs"], 60);
        assert_eq!(j["stats"]["grants"], 1);
        assert_eq!(j["stats"]["wait_p50_ms"], 1_000);
        assert_eq!(j["stats"]["queued"], 1);
        assert_eq!(j["per_device"][0]["busy_ms"], 10_000);
        assert_eq!(j["per_device"][0]["utilisation"], 0.1);
        assert_eq!(j["per_agent"][0]["agent"], "a");
        assert_eq!(pct(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.95), 10);
    }
}
