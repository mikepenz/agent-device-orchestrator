//! `devq skills`: install the bundled agent skill (add | list | update | remove).
use crate::db::Res;
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

const NAME: &str = "devq";
const BODY: &str = include_str!("../skills/devq/SKILL.md");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const K_VER: &str = "  devq-version: ";
// ponytail: FNV-1a 64 instead of sha256 (no new dependency). Detects hand edits only; not tamper-proof.
const K_HASH: &str = "  devq-hash: ";

#[derive(Args, Default, Clone)]
#[command(group(clap::ArgGroup::new("target").args(["project", "dir"])))]
pub struct Target {
    /// Use ./.claude/skills/devq/ in the current directory (default: global ~/.claude/skills/devq/)
    #[arg(long)]
    project: bool,
    /// Use <PATH>/devq/SKILL.md, for other agent tools
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum SkillsCmd {
    /// Show the bundled skill and the install state of the global, project and --dir targets
    List {
        /// Also check <PATH>/devq/SKILL.md
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// Install the skill (creates directories). Refuses modified or foreign copies unless --force
    Add {
        /// Skill name (only `devq` exists)
        name: Option<String>,
        #[command(flatten)]
        target: Target,
        /// Overwrite a locally modified or foreign SKILL.md
        #[arg(long)]
        force: bool,
    },
    /// Rewrite outdated, unmodified copies; modified or foreign ones are reported and skipped (exit 1)
    Update {
        /// Skill name (only `devq` exists)
        name: Option<String>,
        /// Narrow to one target (default: global and project)
        #[command(flatten)]
        target: Target,
    },
    /// Delete the installed skill if it carries the devq marker and is unmodified, unless --force
    Remove {
        /// Skill name (only `devq` exists)
        name: Option<String>,
        #[command(flatten)]
        target: Target,
        /// Delete a modified or foreign SKILL.md
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, PartialEq)]
enum State {
    Missing,
    Current,
    Outdated(String),
    Modified,
    Foreign,
    /// devq's own checkout copy; never touched
    Source,
    /// content equals the bundled skill but no devq metadata (e.g. a plain `cp`)
    Copy,
}

fn hash(s: &str) -> String {
    let h = s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

/// Bundled text with the metadata block inserted after the opening `---`.
fn render(ver: &str) -> String {
    let (first, rest) = BODY.split_once('\n').unwrap_or((BODY, ""));
    format!("{first}\nmetadata:\n{K_VER}{ver}\n{K_HASH}{}\n{rest}", hash(BODY))
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(key)).map(str::trim)
}

fn classify(path: &Path) -> State {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return State::Missing,
        Err(_) => return State::Foreign,
        Ok(_) => {}
    }
    // dangling symlink, directory, unreadable or non-UTF-8: not ours
    let Some(text) = std::fs::read(path).ok().and_then(|b| String::from_utf8(b).ok()) else { return State::Foreign };
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/devq/SKILL.md");
    if matches!((path.canonicalize(), src.canonicalize()), (Ok(a), Ok(b)) if a == b) {
        return State::Source;
    }
    if text == BODY {
        return State::Copy;
    }
    let (Some(v), Some(h)) = (field(&text, K_VER), field(&text, K_HASH)) else { return State::Foreign };
    let stripped: String =
        text.split_inclusive('\n').filter(|l| !(l.trim_end() == "metadata:" || l.starts_with(K_VER) || l.starts_with(K_HASH))).collect();
    if hash(&stripped) != h {
        State::Modified
    } else if v == VERSION && h == hash(BODY) {
        State::Current
    } else {
        State::Outdated(v.to_string())
    }
}

fn describe(s: &State) -> String {
    match s {
        State::Missing => "not installed".into(),
        State::Current => format!("installed v{VERSION}"),
        State::Outdated(v) if v == VERSION => "outdated (content changed)".into(),
        State::Outdated(v) => format!("outdated (v{v} → v{VERSION})"),
        State::Modified => "modified locally".into(),
        State::Foreign => "foreign (not managed)".into(),
        State::Source => "source copy".into(),
        State::Copy => "unmanaged copy of the source".into(),
    }
}

fn file(base: &Path) -> PathBuf {
    base.join(NAME).join("SKILL.md")
}

const SOURCE_MSG: &str = "this is devq's source copy; not touching it";

/// (label, path) for the chosen target; no flags means global, which needs `home`.
fn one(t: &Target, home: Option<&Path>, cwd: &Path) -> Res<(String, PathBuf)> {
    if let Some(d) = &t.dir {
        let d = match (d.strip_prefix("~"), home) {
            (Ok(rest), Some(h)) => h.join(rest),
            (Ok(_), None) => return Err("HOME is not set; cannot expand ~".into()),
            _ => d.clone(),
        };
        Ok((format!("dir {}", d.display()), file(&d)))
    } else if t.project {
        Ok(("project".into(), file(&cwd.join(".claude/skills"))))
    } else {
        let h = home.ok_or("HOME is not set (or empty); use --project or --dir")?;
        Ok(("global".into(), file(&h.join(".claude/skills"))))
    }
}

fn check_name(name: &Option<String>) -> bool {
    match name.as_deref() {
        None | Some(NAME) => true,
        Some(n) => {
            eprintln!("devq: unknown skill '{n}'. Available skills: {NAME}");
            false
        }
    }
}

fn on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(if cfg!(windows) { "devq.exe" } else { "devq" }).is_file()))
}

fn write(path: &Path) -> Res<()> {
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".SKILL.md.tmp{}", std::process::id()));
    std::fs::write(&tmp, render(VERSION)).and_then(|_| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Runs a subcommand against explicit `home` and `cwd`; returns the exit code.
pub fn run(cmd: SkillsCmd, home: Option<&Path>, cwd: &Path) -> Res<i32> {
    match cmd {
        SkillsCmd::List { dir } => {
            println!("{NAME} v{VERSION}");
            let mut ts = vec![Target::default(), Target { project: true, dir: None }];
            if dir.is_some() {
                ts.push(Target { project: false, dir });
            }
            for t in ts {
                match one(&t, home, cwd) {
                    Ok((label, p)) => println!("  {label}: {} ({})", describe(&classify(&p)), p.display()),
                    Err(e) => println!("  {}: unavailable ({e})", if t.dir.is_some() { "dir" } else { "global" }),
                }
            }
            Ok(0)
        }
        SkillsCmd::Add { name, target, force } => {
            if !check_name(&name) {
                return Ok(2);
            }
            let (label, p) = one(&target, home, cwd)?;
            let st = classify(&p);
            if st == State::Source {
                eprintln!("devq: {label}: {SOURCE_MSG}");
                return Ok(1);
            }
            if st == State::Current {
                println!("{label}: up to date");
                return Ok(0);
            }
            if matches!(st, State::Modified | State::Foreign | State::Copy) && !force {
                eprintln!("devq: {label}: {} at {}; use --force to overwrite", describe(&st), p.display());
                return Ok(1);
            }
            write(&p)?;
            println!("{label}: installed v{VERSION} at {}", p.display());
            if !on_path() {
                eprintln!("devq: warning: `devq` was not found on PATH; the skill needs it to work");
            }
            Ok(0)
        }
        SkillsCmd::Update { name, target } => {
            if !check_name(&name) {
                return Ok(2);
            }
            let explicit = target.project || target.dir.is_some();
            let ts = if explicit { vec![target] } else { vec![Target::default(), Target { project: true, dir: None }] };
            let mut code = 0;
            for t in ts {
                let (label, p) = match one(&t, home, cwd) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("devq: global: {e}");
                        code = 1;
                        continue;
                    }
                };
                match classify(&p) {
                    State::Outdated(_) => {
                        write(&p)?;
                        println!("{label}: updated to v{VERSION}");
                    }
                    State::Modified => {
                        eprintln!("devq: {label}: modified locally, skipped; use `devq skills add --force` to restore ({})", p.display());
                        code = 1;
                    }
                    s @ (State::Foreign | State::Copy | State::Source) if explicit => {
                        eprintln!("devq: {label}: {}, skipped ({})", describe(&s), p.display());
                        code = 1;
                    }
                    State::Foreign | State::Copy | State::Source => eprintln!("devq: {label}: not managed by devq, left alone"),
                    s => println!("{label}: {}", describe(&s)),
                }
            }
            Ok(code)
        }
        SkillsCmd::Remove { name, target, force } => {
            if !check_name(&name) {
                return Ok(2);
            }
            let (label, p) = one(&target, home, cwd)?;
            let st = classify(&p);
            if st == State::Source {
                eprintln!("devq: {label}: {SOURCE_MSG}");
                return Ok(1);
            }
            if st == State::Missing {
                println!("{label}: not installed, nothing to remove");
                return Ok(0);
            }
            if matches!(st, State::Modified | State::Foreign | State::Copy) && !force {
                eprintln!("devq: {label}: {} at {}; use --force to delete", describe(&st), p.display());
                return Ok(1);
            }
            std::fs::remove_file(&p)?;
            let _ = std::fs::remove_dir(p.parent().unwrap()); // only succeeds when empty
            println!("{label}: removed {}", p.display());
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("devq-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn add(h: &Path, force: bool) -> i32 {
        run(SkillsCmd::Add { name: None, target: Target::default(), force }, Some(h), h).unwrap()
    }
    fn upd(h: &Path) -> i32 {
        run(SkillsCmd::Update { name: None, target: Target::default() }, Some(h), h).unwrap()
    }
    fn rm(h: &Path, force: bool) -> i32 {
        run(SkillsCmd::Remove { name: None, target: Target::default(), force }, Some(h), h).unwrap()
    }
    fn gp(h: &Path) -> PathBuf {
        file(&h.join(".claude/skills"))
    }

    #[test]
    fn add_then_list_installed_and_up_to_date() {
        let h = tmp("add");
        assert_eq!(classify(&gp(&h)), State::Missing);
        assert_eq!(add(&h, false), 0);
        assert_eq!(classify(&gp(&h)), State::Current);
        assert_eq!(add(&h, false), 0); // up to date
        assert_eq!(run(SkillsCmd::List { dir: None }, Some(&h), &h).unwrap(), 0);
    }

    #[test]
    fn edit_is_modified_update_skips_force_restores() {
        let h = tmp("mod");
        add(&h, false);
        let p = gp(&h);
        let t = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, t + "\nmy edit\n").unwrap();
        assert_eq!(classify(&p), State::Modified);
        assert_eq!(upd(&h), 1);
        assert_eq!(add(&h, false), 1);
        assert_eq!(rm(&h, false), 1);
        assert_eq!(add(&h, true), 0);
        assert_eq!(classify(&p), State::Current);
    }

    #[test]
    fn older_version_is_outdated_then_updated() {
        let h = tmp("old");
        add(&h, false);
        let p = gp(&h);
        let t = std::fs::read_to_string(&p).unwrap().replace(&format!("{K_VER}{VERSION}"), &format!("{K_VER}0.0.1"));
        std::fs::write(&p, t).unwrap();
        assert_eq!(classify(&p), State::Outdated("0.0.1".into()));
        assert_eq!(upd(&h), 0);
        assert_eq!(classify(&p), State::Current);
    }

    #[test]
    fn remove_cleans_dir_and_missing_is_ok() {
        let h = tmp("rm");
        assert_eq!(rm(&h, false), 0);
        add(&h, false);
        assert_eq!(rm(&h, false), 0);
        assert!(!gp(&h).parent().unwrap().exists());
    }

    #[test]
    fn foreign_file_is_protected() {
        let h = tmp("foreign");
        let p = gp(&h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "---\nname: devq\n---\nmine\n").unwrap();
        assert_eq!(classify(&p), State::Foreign);
        assert_eq!(add(&h, false), 1);
        assert_eq!(upd(&h), 0);
        assert_eq!(rm(&h, false), 1);
        assert!(p.exists());
        assert_eq!(rm(&h, true), 0);
        assert!(!p.exists());
    }

    #[test]
    fn project_dir_and_unknown_name() {
        let h = tmp("targets");
        let cwd = h.join("proj");
        let t = Target { project: true, dir: None };
        run(SkillsCmd::Add { name: Some("devq".into()), target: t, force: false }, Some(&h), &cwd).unwrap();
        assert_eq!(classify(&file(&cwd.join(".claude/skills"))), State::Current);
        assert_eq!(classify(&gp(&h)), State::Missing);
        let d = Target { project: false, dir: Some(h.join("other")) };
        run(SkillsCmd::Add { name: None, target: d, force: false }, Some(&h), &cwd).unwrap();
        assert_eq!(classify(&h.join("other/devq/SKILL.md")), State::Current);
        let bad = SkillsCmd::Add { name: Some("nope".into()), target: Target::default(), force: false };
        assert_eq!(run(bad, Some(&h), &cwd).unwrap(), 2);
        // no HOME: global errors, project still works
        let g = SkillsCmd::Add { name: None, target: Target::default(), force: false };
        assert!(run(g, None, &cwd).is_err());
        let pr = SkillsCmd::Add { name: None, target: Target { project: true, dir: None }, force: false };
        assert_eq!(run(pr, None, &cwd).unwrap(), 0);
    }

    #[test]
    fn non_utf8_and_directory_are_foreign_and_protected() {
        let h = tmp("odd");
        let p = gp(&h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(classify(&p), State::Foreign);
        assert_eq!(add(&h, false), 1);
        assert_eq!(rm(&h, false), 1);
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert_eq!(classify(&p), State::Foreign);
        assert_eq!(add(&h, false), 1);
        assert!(p.is_dir());
    }

    #[test]
    fn content_only_copy_is_forceable() {
        let h = tmp("cp");
        let p = gp(&h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, BODY).unwrap();
        assert_eq!(classify(&p), State::Copy);
        assert_eq!(add(&h, false), 1);
        assert_eq!(upd(&h), 0);
        assert_eq!(add(&h, true), 0);
        assert_eq!(classify(&p), State::Current);
        std::fs::write(&p, BODY).unwrap();
        assert_eq!(rm(&h, true), 0);
        assert!(!p.exists());
    }

    #[test]
    fn source_copy_is_never_touched() {
        let h = tmp("src");
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let t = || Target { project: false, dir: Some(repo.join("skills")) };
        let before = std::fs::read(repo.join("skills/devq/SKILL.md")).unwrap();
        let add = SkillsCmd::Add { name: None, target: t(), force: true };
        assert_eq!(run(add, Some(&h), repo).unwrap(), 1);
        let rm = SkillsCmd::Remove { name: None, target: t(), force: true };
        assert_eq!(run(rm, Some(&h), repo).unwrap(), 1);
        assert_eq!(std::fs::read(repo.join("skills/devq/SKILL.md")).unwrap(), before);
    }

    #[test]
    fn bundled_body_has_no_metadata() {
        assert!(BODY.starts_with("---\n"));
        assert!(!BODY.lines().any(|l| l.trim_end() == "metadata:"));
    }

    #[test]
    fn default_update_ignores_foreign_but_explicit_fails() {
        let h = tmp("upd");
        let p = gp(&h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "mine\n").unwrap();
        assert_eq!(upd(&h), 0);
        let t = Target { project: false, dir: Some(h.join(".claude/skills")) };
        assert_eq!(run(SkillsCmd::Update { name: None, target: t }, Some(&h), &h).unwrap(), 1);
    }
}
