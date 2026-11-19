//! `devq hook`: optional PreToolUse/BeforeTool guardrail that blocks raw device commands.
//! A guardrail, not a security boundary (a determined agent can evade a text-level check).
use crate::db::{now_ms, Res};
use crate::pool;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum HookCmd {
    /// Add the hook to the agent CLI's settings (idempotent; keeps all other keys)
    Install(InstallArgs),
    /// Remove exactly what `install` added
    Uninstall(InstallArgs),
    /// Print `allow` or `deny` (+ reason) for a command, without reading stdin
    Test {
        /// The shell command line to check
        command: String,
    },
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Debug)]
pub enum Tool {
    Claude,
    Codex,
    Gemini,
}

#[derive(Args, Clone)]
#[command(group(clap::ArgGroup::new("scope").args(["project", "global", "dir"])))]
pub struct InstallArgs {
    /// Use ./.claude/settings.json etc. in the current directory
    #[arg(long)]
    project: bool,
    /// Use ~/.claude/settings.json etc. (default)
    #[arg(long)]
    global: bool,
    /// Use <PATH>/.claude/settings.json etc.
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
    /// Which agent CLI's settings to edit
    #[arg(long, value_enum, default_value = "claude")]
    target: Tool,
    /// (claude only) also add/remove the static `permissions.deny` backstop rules
    #[arg(long)]
    deny_rules: bool,
    /// Install the hook as `devq hook --strict` (internal errors block instead of passing)
    #[arg(long)]
    strict: bool,
}

const MSG_TAIL: &str = "no devq slot for this device command. Load the `devq` skill and use `devq run --agent <name> --purpose \"<why>\" -- <cmd>`, or `devq request` … `adb -s <serial>` … `devq release <slot>`.";
const SHELLS: [&str; 7] = ["sh", "bash", "zsh", "dash", "ksh", "fish", "su"];
const EXEMPT: [&str; 8] = ["devices", "version", "help", "start-server", "kill-server", "--version", "--help", "-h"];
// Static backstop. Prefix-anchored on purpose: a `*connected*` wildcard anywhere would also block `devq run -- ./gradlew connected…`.
const DENY_RULES: [&str; 4] = ["Bash(adb *)", "Bash(./gradlew *connected*)", "Bash(gradlew *connected*)", "Bash(gradle *connected*)"];

// ---------- splitter ----------

fn matching(c: &[char], mut i: usize, open: char, close: char) -> usize {
    // c[i] is just past the opener; returns index of the matching closer (or end).
    let mut d = 1;
    while i < c.len() {
        if c[i] == open {
            d += 1;
        } else if c[i] == close {
            d -= 1;
            if d == 0 {
                return i;
            }
        }
        i += 1;
    }
    c.len()
}

/// Splits into simple commands (words, quotes removed), flattening `$(..)` and backticks.
/// ponytail: not a shell; heredocs, `{}` groups and `$((..))` arithmetic are approximated.
#[allow(unused_assignments)] // the final macro expansion resets `has` without reading it
fn split(s: &str, out: &mut Vec<Vec<String>>) {
    let c: Vec<char> = s.chars().collect();
    let (mut cmd, mut w, mut has) = (Vec::<String>::new(), String::new(), false);
    let mut i = 0;
    macro_rules! word {
        () => {
            if has {
                cmd.push(std::mem::take(&mut w));
                has = false;
            }
        };
    }
    macro_rules! cmd {
        () => {{
            word!();
            if !cmd.is_empty() {
                out.push(std::mem::take(&mut cmd));
            }
        }};
    }
    while i < c.len() {
        match c[i] {
            '\'' => {
                has = true;
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    w.push(c[i]);
                    i += 1;
                }
            }
            '"' => {
                has = true;
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' && i + 1 < c.len() {
                        i += 1;
                    } else if c[i] == '$' && c.get(i + 1) == Some(&'(') {
                        let e = matching(&c, i + 2, '(', ')');
                        split(&c[i + 2..e].iter().collect::<String>(), out);
                        w.push_str("$(…)");
                        i = e + 1;
                        continue;
                    } else if c[i] == '`' {
                        let e = c[i + 1..].iter().position(|&x| x == '`').map_or(c.len(), |p| i + 1 + p);
                        split(&c[i + 1..e].iter().collect::<String>(), out);
                        w.push_str("`…`");
                        i = e + 1;
                        continue;
                    }
                    w.push(c[i]);
                    i += 1;
                }
            }
            '\\' if i + 1 < c.len() => {
                has = true;
                i += 1;
                w.push(c[i]);
            }
            '$' if c.get(i + 1) == Some(&'(') => {
                let e = matching(&c, i + 2, '(', ')');
                split(&c[i + 2..e].iter().collect::<String>(), out);
                has = true;
                w.push_str("$(…)");
                i = e;
            }
            '`' => {
                let e = c[i + 1..].iter().position(|&x| x == '`').map_or(c.len(), |p| i + 1 + p);
                split(&c[i + 1..e].iter().collect::<String>(), out);
                has = true;
                w.push_str("`…`");
                i = e;
            }
            '&' if i > 0 && c[i - 1] == '>' || c.get(i + 1) == Some(&'>') => {
                has = true;
                w.push('&'); // 2>&1, &>file
            }
            ';' | '\n' | '|' | '&' | '(' | ')' => cmd!(),
            x if x.is_whitespace() => word!(),
            x => {
                has = true;
                w.push(x);
            }
        }
        i += 1;
    }
    cmd!();
}

// ---------- analysis ----------

struct Cx<'a> {
    /// (device, devq agent) of every active slot
    active: &'a [(String, String)],
    env: Option<&'a str>,
    /// DEVQ_AGENT of the hook environment. ponytail: when unset, any active slot on the serial counts
    /// (Claude's `agent_type` is not the devq agent name); set DEVQ_AGENT per agent to get per-agent checks.
    agent: Option<&'a str>,
}

impl Cx<'_> {
    fn holds(&self, dev: Option<&str>) -> bool {
        self.active.iter().any(|(d, a)| dev.is_none_or(|x| x == d) && self.agent.is_none_or(|x| x == a))
    }
}

fn base(p: &str) -> &str {
    let b = p.rsplit(['/', '\\']).next().unwrap_or(p);
    b.strip_suffix(".exe").unwrap_or(b)
}

fn is_assign(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn serial_ok(s: Option<&str>, cx: &Cx) -> bool {
    match s {
        None => false,
        Some("$ANDROID_SERIAL" | "${ANDROID_SERIAL}") => cx.env.is_some_and(|e| cx.holds(Some(e))),
        // ponytail: an unresolvable variable (e.g. $SERIAL) passes if ANY slot is active; resolve per-session if abused
        Some(v) if v.contains('$') => cx.holds(None),
        Some(v) => cx.holds(Some(v)),
    }
}

/// Skips a wrapper's own flags (and the duration for `timeout`).
fn skip_wrapper<'a>(name: &str, mut w: &'a [String]) -> &'a [String] {
    let takes_arg: &[&str] = match name {
        "nice" => &["-n"],
        "sudo" => &["-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U"],
        "doas" => &["-u", "-C"],
        "watch" => &["-n", "--interval"],
        "env" => &["-u", "-C", "--unset", "--chdir"],
        "timeout" => &["-s", "-k"],
        "xargs" => &["-n", "-I", "-P", "-L", "-s", "-d", "-E"],
        _ => &[],
    };
    while let Some(f) = w.first() {
        if !f.starts_with('-') || f == "--" {
            break;
        }
        w = if takes_arg.contains(&f.as_str()) && w.len() > 1 { &w[2..] } else { &w[1..] };
    }
    if name == "timeout" && !w.is_empty() {
        w = &w[1..];
    }
    w
}

/// Some(snippet) when this simple command is an unauthorised device command.
fn check_simple(mut w: &[String], mut serial: Option<String>, cx: &Cx) -> Option<String> {
    let line = w.join(" ");
    let mut xargs = false;
    loop {
        // shell keywords and group openers
        if w.first().is_some_and(|k| matches!(k.as_str(), "{" | "}" | "!" | "then" | "do" | "else" | "elif" | "if" | "while" | "until")) {
            w = &w[1..];
            continue;
        }
        while let Some(f) = w.first().filter(|f| is_assign(f)) {
            if let Some(v) = f.strip_prefix("ANDROID_SERIAL=") {
                serial = Some(v.to_string());
            }
            w = &w[1..];
        }
        let p = w.first()?;
        let b = base(p);
        match b {
            "time" | "nohup" | "env" | "exec" | "nice" | "timeout" | "xargs" | "sudo" | "doas" | "watch" => {
                if b == "env" {
                    // `env -S "adb shell"` splits the string itself: rescan it with the rest
                    let fl = w[1..].iter().take_while(|a| a.starts_with('-')).count();
                    if let Some(i) = w[1..1 + fl].iter().position(|a| a == "-S" || a == "--split-string") {
                        let rest = w[i + 2..].join(" ");
                        return scan(&rest, serial, cx).map(|_| line);
                    }
                }
                xargs |= b == "xargs";
                w = skip_wrapper(b, &w[1..]);
            }
            "command" => {
                if w.get(1).is_some_and(|f| f.starts_with('-')) {
                    return None; // `command -v adb` is a lookup
                }
                w = &w[1..];
            }
            _ => break,
        }
    }
    let p = w.first()?;
    let (b, args) = (base(p), &w[1..]);
    let flagged = if b == "eval" {
        return scan(&args.join(" "), serial, cx).map(|_| line);
    } else if SHELLS.contains(&b) {
        // -c, or combined flags such as -lc / -xc
        let inner = args
            .iter()
            .position(|a| {
                a.len() > 1
                    && a.starts_with('-')
                    && !a.starts_with("--")
                    && a[1..].chars().all(|c| c.is_ascii_alphabetic())
                    && a.contains('c')
            })
            .and_then(|i| args.get(i + 1));
        return inner.and_then(|s| scan(s, serial.clone(), cx));
    } else if b == "adb" {
        let (mut i, mut sub) = (0, None);
        while i < args.len() {
            match args[i].as_str() {
                "-s" => {
                    serial = args.get(i + 1).cloned();
                    i += 2;
                }
                "-t" | "-H" | "-P" | "-L" => i += 2,
                a if a.starts_with('-') && a != "--version" && a != "--help" && a != "-h" => i += 1,
                a => {
                    sub = Some(a);
                    break;
                }
            }
        }
        match sub {
            Some(s) => !EXEMPT.contains(&s),
            None => xargs, // bare `adb` is help, but `… | xargs adb` supplies the subcommand
        }
    } else if b == "am" {
        args.first().is_some_and(|a| a == "instrument")
    } else if matches!(b, "gradlew" | "gradlew.bat" | "gradle") {
        args.iter().any(|a| {
            // ponytail: Gradle's camel-case abbreviation (`cAT`) is not recognised
            a.rsplit(':').next().unwrap_or(a).starts_with("connected")
        })
    } else if b == "emulator" {
        args.iter().any(|a| a == "-avd")
    } else {
        false
    };
    // `adb shell am instrument …` is already caught by the adb branch.
    if flagged && !serial_ok(serial.as_deref(), cx) {
        return Some(line);
    }
    None
}

fn scan(cmd: &str, serial: Option<String>, cx: &Cx) -> Option<String> {
    let mut v = vec![];
    split(cmd, &mut v);
    v.iter().find_map(|w| check_simple(w, serial.clone(), cx))
}

/// None = allow; Some(snippet) = deny. `devq …` is allowed per simple command, so `devq run -- adb …` passes
/// (adb is an argument there) while `devq status && adb shell` does not.
pub fn verdict(cmd: &str, active: &[(String, String)], env_serial: Option<&str>, agent: Option<&str>) -> Option<String> {
    scan(cmd, None, &Cx { active, env: env_serial, agent })
}

// ---------- db ----------

fn active_serials(home: &Path) -> Res<Vec<(String, String)>> {
    let p = home.join("devq.db");
    if !p.exists() {
        return Ok(vec![]);
    }
    let c =
        rusqlite::Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    c.busy_timeout(std::time::Duration::from_millis(300))?;
    let mut st = c.prepare("SELECT device,agent FROM slots WHERE state='active' AND device IS NOT NULL")?;
    let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

fn record_block(agent: &str, command: &str, reason: &str) -> Res<()> {
    let c = crate::db::open(&pool::devq_home())?;
    let cmd: String = command.chars().take(200).collect();
    c.execute("DELETE FROM blocked WHERE at<?", [now_ms() - 30 * 86_400_000])?;
    c.execute("INSERT INTO blocked(at,agent,command,reason) VALUES(?,?,?,?)", rusqlite::params![now_ms(), agent, cmd, reason])?;
    Ok(())
}

fn snip(s: &str) -> String {
    let t: String = s.chars().take(80).collect();
    if t.len() < s.len() {
        format!("{t}…")
    } else {
        t
    }
}

fn message(snippet: &str) -> String {
    format!("devq: blocked `{}`: {MSG_TAIL}", snip(snippet))
}

// ---------- hook entry ----------

fn judge(stdin: &str) -> Res<Option<(String, String, String)>> {
    let v: Value = serde_json::from_str(stdin)?;
    if !matches!(v["tool_name"].as_str(), Some("Bash" | "run_shell_command")) {
        return Ok(None);
    }
    let Some(cmd) = v["tool_input"]["command"].as_str().filter(|c| !c.trim().is_empty()) else { return Ok(None) };
    let active = active_serials(&pool::devq_home())?;
    let env = std::env::var("ANDROID_SERIAL").ok();
    Ok(verdict(cmd, &active, env.as_deref(), std::env::var("DEVQ_AGENT").ok().as_deref()).map(|snippet| {
        let agent =
            v["agent_type"].as_str().map(String::from).or_else(|| std::env::var("DEVQ_AGENT").ok()).unwrap_or_else(|| "unknown".into());
        (snippet, agent, cmd.to_string())
    }))
}

fn from_stdin(strict: bool) -> i32 {
    let mut s = String::new();
    let r = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(Into::into).and_then(|_| judge(&s));
    match r {
        Ok(None) => 0,
        Ok(Some((snippet, agent, cmd))) => {
            if let Err(e) = record_block(&agent, &cmd, "no devq slot for this device command") {
                eprintln!("devq hook: could not record block: {e}");
            }
            eprintln!("{}", message(&snippet));
            2
        }
        Err(e) if strict => {
            eprintln!("devq hook: internal error, blocking (--strict): {e}");
            2
        }
        Err(e) => {
            eprintln!("devq hook: internal error, allowing: {e}");
            0
        }
    }
}

pub fn run(strict: bool, cmd: Option<HookCmd>, home: Option<&Path>, cwd: &Path) -> Res<i32> {
    match cmd {
        None => Ok(from_stdin(strict)),
        Some(HookCmd::Test { command }) => {
            let active = active_serials(&pool::devq_home()).unwrap_or_default();
            match verdict(&command, &active, std::env::var("ANDROID_SERIAL").ok().as_deref(), std::env::var("DEVQ_AGENT").ok().as_deref()) {
                None => println!("allow"),
                Some(s) => println!("deny\n{}", message(&s)),
            }
            Ok(0)
        }
        Some(HookCmd::Install(a)) => edit(&a, true, home, cwd).map(|_| 0),
        Some(HookCmd::Uninstall(a)) => edit(&a, false, home, cwd).map(|_| 0),
    }
}

// ---------- installer ----------

fn settings_path(a: &InstallArgs, home: Option<&Path>, cwd: &Path) -> Res<PathBuf> {
    let base = if let Some(d) = &a.dir {
        match (d.strip_prefix("~"), home) {
            (Ok(rest), Some(h)) => h.join(rest),
            (Ok(_), None) => return Err("HOME is not set; cannot expand ~".into()),
            _ => d.clone(),
        }
    } else if a.project {
        cwd.to_path_buf()
    } else {
        home.ok_or("HOME is not set (or empty); use --project or --dir")?.to_path_buf()
    };
    Ok(match a.target {
        Tool::Claude => base.join(".claude/settings.json"),
        Tool::Codex => base.join(".codex/hooks.json"),
        Tool::Gemini => base.join(".gemini/settings.json"),
    })
}

/// (event, matcher, hook entry). Codex and Gemini shapes are taken from their docs, not verified against a live CLI.
fn entry(t: Tool, command: &str) -> (&'static str, &'static str, Value) {
    match t {
        Tool::Claude => ("PreToolUse", "Bash", json!({"type": "command", "command": command, "timeout": 10})),
        Tool::Codex => ("PreToolUse", "^Bash$", json!({"type": "command", "command": command, "timeout": 10})),
        // Gemini timeouts are milliseconds
        Tool::Gemini => {
            ("BeforeTool", "run_shell_command", json!({"name": "devq", "type": "command", "command": command, "timeout": 10000}))
        }
    }
}

fn is_ours(h: &Value) -> bool {
    // exactly `<path>/devq[.exe] hook [--strict]`; user hooks like `devq hook-lint` or `echo devq hook` are not ours
    let mut v = vec![];
    split(h["command"].as_str().unwrap_or(""), &mut v);
    matches!(v.as_slice(), [w] if w.len() >= 2 && base(&w[0]) == "devq" && w[1] == "hook" && w[2..].iter().all(|a| a == "--strict"))
}

fn hook_command(strict: bool) -> Res<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let p = if cfg!(test) { std::borrow::Cow::Borrowed("devq") } else { exe.to_string_lossy() }; // test binary has another name
    let p = if p.contains(' ') { format!("\"{p}\"") } else { p.to_string() };
    Ok(format!("{p} hook{}", if strict { " --strict" } else { "" }))
}

/// Pure JSON edit; `Err` when the existing structure is not what we can safely merge into.
fn apply(root: &mut Value, a: &InstallArgs, command: &str, install: bool) -> Result<(), String> {
    let (event, matcher, hook) = entry(a.target, command);
    let obj = root.as_object_mut().ok_or("top level is not a JSON object")?;
    if install {
        let arr = obj
            .entry("hooks")
            .or_insert(json!({}))
            .as_object_mut()
            .ok_or("`hooks` is not an object")?
            .entry(event)
            .or_insert(json!([]))
            .as_array_mut()
            .ok_or("hooks event is not an array")?;
        let mut found = false;
        for h in arr.iter_mut().filter_map(|g| g.get_mut("hooks")?.as_array_mut()).flatten().filter(|h| is_ours(h)) {
            *h = hook.clone();
            found = true;
        }
        if !found {
            arr.push(json!({"matcher": matcher, "hooks": [hook]}));
        }
        if a.deny_rules && a.target == Tool::Claude {
            let deny = obj
                .entry("permissions")
                .or_insert(json!({}))
                .as_object_mut()
                .ok_or("`permissions` is not an object")?
                .entry("deny")
                .or_insert(json!([]))
                .as_array_mut()
                .ok_or("`permissions.deny` is not an array")?;
            for r in DENY_RULES {
                if !deny.iter().any(|d| d == r) {
                    deny.push(json!(r));
                }
            }
        }
        return Ok(());
    }
    if let Some(arr) = obj.get_mut("hooks").and_then(|h| h.get_mut(event)).and_then(Value::as_array_mut) {
        for g in arr.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                let n = hs.len();
                hs.retain(|h| !is_ours(h));
                if hs.is_empty() && n > 0 {
                    *g = Value::Null; // group held only our hook
                }
            }
        }
        arr.retain(|g| !g.is_null());
        if arr.is_empty() {
            obj["hooks"].as_object_mut().unwrap().remove(event);
        }
        if obj["hooks"].as_object().is_some_and(|h| h.is_empty()) {
            obj.remove("hooks");
        }
    }
    if a.deny_rules && a.target == Tool::Claude {
        if let Some(deny) = obj.get_mut("permissions").and_then(|p| p.get_mut("deny")).and_then(Value::as_array_mut) {
            deny.retain(|d| !DENY_RULES.iter().any(|r| d == r));
            if deny.is_empty() {
                obj["permissions"].as_object_mut().unwrap().remove("deny");
            }
        }
        if obj.get("permissions").and_then(Value::as_object).is_some_and(|p| p.is_empty()) {
            obj.remove("permissions");
        }
    }
    Ok(())
}

fn edit(a: &InstallArgs, install: bool, home: Option<&Path>, cwd: &Path) -> Res<()> {
    if a.deny_rules && a.target != Tool::Claude {
        return Err("--deny-rules only applies to --target claude".into());
    }
    let path = settings_path(a, home, cwd)?;
    let existed = path.exists();
    let mut root: Value = if existed {
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|e| format!("{} is not valid JSON ({e}); refusing to touch it", path.display()))?
    } else {
        json!({})
    };
    if !install && !existed {
        println!("nothing to remove: {} does not exist", path.display());
        return Ok(());
    }
    let command = if install { hook_command(a.strict)? } else { String::new() };
    apply(&mut root, a, &command, install).map_err(|e| format!("{}: {e}; refusing to touch it", path.display()))?;
    if !install && root.as_object().is_some_and(|o| o.is_empty()) {
        std::fs::remove_file(&path)?;
        println!("removed devq hook; deleted empty {}", path.display());
        return Ok(());
    }
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".devq-hook.tmp{}", std::process::id()));
    let text = serde_json::to_string_pretty(&root)? + "\n";
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    println!("{} devq hook in {}", if install { "installed" } else { "removed" }, path.display());
    if install && a.target == Tool::Codex {
        eprintln!("devq: note: Codex runs non-managed hooks only after you review and trust them via `/hooks`");
    }
    if install && a.target != Tool::Claude {
        eprintln!("devq: note: the {:?} hook schema is taken from its docs and not verified against a live CLI", a.target);
    }
    if install && a.deny_rules {
        eprintln!("devq: note: deny rules are prefix-anchored (`adb *`), so `devq run … -- adb …` still passes, but a bare `adb …` is blocked even inside an allowed slot; use `devq run`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(v: &[&str]) -> Vec<(String, String)> {
        v.iter().map(|s| (s.to_string(), "a".to_string())).collect()
    }

    #[test]
    fn table() {
        let none = act(&[]);
        let slot = act(&["emulator-5554"]);
        // (command, active slots, env ANDROID_SERIAL, expect deny)
        type Slots = Vec<(String, String)>;
        let t: Vec<(&str, &Slots, Option<&str>, bool)> = vec![
            ("adb shell getprop", &none, None, true),
            ("cd app && adb install x.apk", &none, None, true),
            ("./gradlew :app:connectedDebugAndroidTest", &none, None, true),
            ("./gradlew connectedCheckTest", &none, None, true),
            ("gradle :app:connectedDebugAndroidTest --info", &none, None, true),
            ("sh -c 'adb logcat -d'", &none, None, true),
            ("bash -c \"cd x; adb shell ls\"", &none, None, true),
            ("/opt/sdk/platform-tools/adb install a.apk", &none, None, true),
            ("/opt/sdk/platform-tools/adb devices", &none, None, false),
            ("adb -s emulator-5554 shell ls", &none, None, true),
            ("adb -s emulator-5554 shell ls", &slot, None, false),
            ("adb -s emulator-5556 shell ls", &slot, None, true),
            ("ANDROID_SERIAL=emulator-5554 adb shell", &slot, None, false),
            ("ANDROID_SERIAL=emulator-5554 adb shell", &none, None, true),
            ("adb -s \"$ANDROID_SERIAL\" shell ls", &slot, Some("emulator-5554"), false),
            ("adb -s $ANDROID_SERIAL shell ls", &slot, None, true),
            ("adb -s $ANDROID_SERIAL shell ls", &none, Some("emulator-5554"), true),
            ("adb -s $SERIAL shell ls", &slot, None, false),
            ("adb -s $SERIAL shell ls", &none, None, true),
            ("devq run --agent a --purpose p -- adb shell", &none, None, false),
            ("devq run --agent a --purpose p -- ./gradlew connectedDebugAndroidTest", &none, None, false),
            ("devq status && adb shell", &none, None, true),
            ("cd app && devq run --agent a --purpose p -- adb shell", &none, None, false),
            ("echo adb", &none, None, false),
            ("grep adb file", &none, None, false),
            ("git commit -m \"fix adb\"", &none, None, false),
            ("echo 'adb shell'", &none, None, false),
            ("adb devices", &none, None, false),
            ("adb version", &none, None, false),
            ("adb start-server", &none, None, false),
            ("adb", &none, None, false),
            ("timeout 5 adb shell", &none, None, true),
            ("nohup env FOO=1 adb logcat", &none, None, true),
            ("time adb shell", &none, None, true),
            ("echo $(adb shell getprop)", &none, None, true),
            ("echo `adb shell getprop`", &none, None, true),
            ("echo \"$(adb shell getprop)\"", &none, None, true),
            ("echo shell | xargs adb", &none, None, true),
            ("adb shell getprop | grep model", &none, None, true),
            ("ls; adb logcat", &none, None, true),
            ("true || adb shell", &none, None, true),
            ("(adb shell ls)", &none, None, true),
            ("ls\nadb shell", &none, None, true),
            ("adb shell am instrument -w pkg/Runner", &none, None, true),
            ("am instrument -w pkg/Runner", &none, None, true),
            ("am start x", &none, None, false),
            ("./gradlew test", &none, None, false),
            ("./gradlew assembleDebug", &none, None, false),
            ("./gradlew :app:testDebugUnitTest", &none, None, false),
            ("emulator -avd Pixel", &none, None, true),
            ("emulator -list-avds", &none, None, false),
            ("adb shell ls 2>&1", &none, None, true),
            ("echo hi > adb.txt", &none, None, false),
            ("command -v adb", &none, None, false),
            ("cat adb.log | head", &none, None, false),
            ("", &none, None, false),
            ("bash -lc 'adb shell'", &none, None, true),
            ("sh -xc \"adb shell\"", &none, None, true),
            ("bash -ec 'adb shell'", &none, None, true),
            ("ksh -c 'adb shell'", &none, None, true),
            ("fish -c 'adb shell'", &none, None, true),
            ("su -c 'adb shell'", &none, None, true),
            ("sudo -u bob adb shell", &none, None, true),
            ("sudo -g wheel -u bob adb shell", &none, None, true),
            ("doas -u bob adb shell", &none, None, true),
            ("watch -n 2 adb shell", &none, None, true),
            ("env -u FOO adb shell", &none, None, true),
            ("env -C /tmp adb shell", &none, None, true),
            ("env -S \"adb shell\"", &none, None, true),
            ("eval \"adb shell\"", &none, None, true),
            ("eval adb shell", &none, None, true),
            ("{ adb shell; }", &none, None, true),
            ("if true; then adb shell; fi", &none, None, true),
            ("for i in 1 2; do adb shell; done", &none, None, true),
            ("while true; do adb logcat; done", &none, None, true),
            ("! adb shell", &none, None, true),
            ("./gradlew connectedCheck", &none, None, true),
            ("./gradlew :app:connectedCheck", &none, None, true),
            ("./gradlew -p app connectedCheck", &none, None, true),
            ("adb.exe shell", &none, None, true),
            ("C:\\sdk\\adb.exe devices", &none, None, false),
            ("adb --help", &none, None, false),
            ("adb -h", &none, None, false),
            ("sudo ls", &none, None, false),
            ("bash -c 'ls'", &none, None, false),
            ("eval \"echo adb\"", &none, None, false),
        ];
        assert!(t.len() >= 30);
        let bad: Vec<_> = t.iter().filter(|(c, a, e, deny)| verdict(c, a, *e, None).is_some() != *deny).map(|t| t.0).collect();
        assert!(bad.is_empty(), "wrong verdicts: {bad:#?}");
    }

    #[test]
    fn active_from_temp_db() {
        let d = std::env::temp_dir().join(format!("devq-hook-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(active_serials(&d).unwrap().is_empty()); // no db yet
        let c = crate::db::open(&d).unwrap();
        c.execute(
            "INSERT INTO slots(agent,purpose,expect_secs,max_secs,state,device,requested_at,heartbeat_at) VALUES('a','p',1,2,'active','emulator-5554',0,0)",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO blocked(at,agent,command,reason) VALUES(1,'a','adb','r')", []).unwrap();
        let a = active_serials(&d).unwrap();
        assert_eq!(a, vec![("emulator-5554".to_string(), "a".to_string())]);
        assert!(verdict("adb -s emulator-5554 shell ls", &a, None, None).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn serial_is_per_agent_when_devq_agent_set() {
        let slots = vec![("emulator-5554".to_string(), "alice".to_string())];
        let c = "adb -s emulator-5554 shell ls";
        assert!(verdict(c, &slots, None, Some("alice")).is_none());
        assert!(verdict(c, &slots, None, Some("bob")).is_some(), "another agent's slot");
        assert!(verdict(c, &slots, None, None).is_none(), "accepted limit: DEVQ_AGENT unset");
        assert!(verdict("adb -s $S shell", &slots, None, Some("bob")).is_some());
    }

    #[test]
    fn user_hooks_with_similar_commands_survive() {
        let mine = |c: &str| json!({"type": "command", "command": c});
        let mut v = json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [mine("/x/devq hook-lint"), mine("echo devq hook")]}]}});
        let before = v.clone();
        let a = args(Tool::Claude, false);
        apply(&mut v, &a, "/bin/devq hook", true).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        apply(&mut v, &a, "", false).unwrap();
        assert_eq!(v, before);
        assert!(is_ours(&mine("\"/a b/devq\" hook --strict")) && is_ours(&mine("devq.exe hook")));
        assert!(!is_ours(&mine("devq hook --other")));
    }

    fn args(t: Tool, deny: bool) -> InstallArgs {
        InstallArgs { project: true, global: false, dir: None, target: t, deny_rules: deny, strict: false }
    }

    #[test]
    fn installer_merge_idempotent_uninstall() {
        let orig = json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(ls)"], "deny": ["Bash(rm *)"]},
            "hooks": {"PreToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "lint"}]}],
                      "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "x"}]}]}
        });
        for t in [Tool::Claude, Tool::Codex, Tool::Gemini] {
            let a = args(t, t == Tool::Claude);
            let mut v = orig.clone();
            if t == Tool::Gemini {
                v = json!({"theme": "x", "hooks": {"BeforeTool": [{"matcher": "other", "hooks": []}]}});
            }
            let before = v.clone();
            apply(&mut v, &a, "/bin/devq hook", true).unwrap();
            let once = v.clone();
            assert_ne!(once, before);
            apply(&mut v, &a, "/bin/devq hook", true).unwrap();
            assert_eq!(v, once, "idempotent");
            let ev = entry(t, "").0;
            assert_eq!(v["hooks"][ev].as_array().unwrap().iter().filter(|g| g["hooks"].as_array().unwrap().iter().any(is_ours)).count(), 1);
            apply(&mut v, &a, "", false).unwrap();
            assert_eq!(v, before, "uninstall restores {t:?}");
        }
        // empty file round trip leaves nothing
        let mut v = json!({});
        apply(&mut v, &args(Tool::Claude, true), "devq hook", true).unwrap();
        apply(&mut v, &args(Tool::Claude, true), "", false).unwrap();
        assert_eq!(v, json!({}));
        // strict re-install replaces the command in place
        let mut v = json!({});
        apply(&mut v, &args(Tool::Claude, false), "devq hook", true).unwrap();
        apply(&mut v, &args(Tool::Claude, false), "devq hook --strict", true).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "devq hook --strict");
        // wrong shapes are refused
        assert!(apply(&mut json!([]), &args(Tool::Claude, false), "c", true).is_err());
        assert!(apply(&mut json!({"hooks": 3}), &args(Tool::Claude, false), "c", true).is_err());
    }

    #[test]
    fn installer_files() {
        let d = std::env::temp_dir().join(format!("devq-hook-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        let p = d.join(".claude/settings.json");
        std::fs::write(&p, "{ not json").unwrap();
        let a = args(Tool::Claude, false);
        assert!(edit(&a, true, None, &d).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json", "invalid JSON left untouched");
        std::fs::write(&p, r#"{"a":1}"#).unwrap();
        edit(&a, true, None, &d).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().ends_with(" hook"));
        edit(&a, false, None, &d).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(&p).unwrap()).unwrap(), json!({"a": 1}));
        let _ = std::fs::remove_dir_all(&d);
    }
}
