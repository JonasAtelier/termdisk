// SPDX-License-Identifier: GPL-3.0-only

//! The termdisk CLI: the view in the terminal, or `--summary`. All terminal code lives here.

use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::Terminal;
use termdisk::tree::MOUNT;
use termdisk::{fmt_size, Colors, Depth, IconSet, NodeId, Outcome, Policy, View};

const USAGE: &str = "usage: termdisk [--read-only] [PATH]
       termdisk --summary [PATH]

  PATH          scan PATH at once (without it: choose home, cache, /tmp or the trash)
  --read-only   look only: nothing can be moved to the trash
  --summary     scan PATH (default: the current folder), print its totals and exit

  TERMDISK_COLORS=truecolor|256|16   the terminal's colours (default: from the environment)
  TERMDISK_ICONS=nerd|unicode|ascii  the glyphs (default: nerd when a Nerd Font is installed)
";

/// How long the icon set's detection may take before Unicode stays.
const ICONS_WAIT: Duration = Duration::from_secs(2);
/// How often the loop looks for the detection's answer while it is out.
const ICONS_CHECK: Duration = Duration::from_millis(15);

/// glibc maps blocks from 128 KiB up on their own, so freeing them gives the memory back, but
/// raises that limit to the size of each one freed (up to 32 MiB). Once a tree's 20 MB of names
/// is freed (a scan again drops the last tree), the next tree grows inside the heap, copied at
/// each step and kept there: a scan again peaked 40 MB above the first. A fixed limit keeps the
/// peak and the memory after it where the first scan left them. The program's own setting, not
/// the library's: a host such as termcode keeps its own.
fn keep_large_blocks_mapped() {
    // SAFETY: mallopt only changes when malloc maps a block; called before any thread starts.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
    }
}

fn main() -> ExitCode {
    keep_large_blocks_mapped();
    let (mut summary, mut read_only, mut path) = (false, false, None);
    for a in std::env::args_os().skip(1) {
        match a.to_str() {
            Some("--summary") => summary = true,
            Some("--read-only") => read_only = true,
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some(s) if s.starts_with('-') => {
                eprint!("termdisk: unknown option {s}\n{USAGE}");
                return ExitCode::from(2);
            }
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => {
                eprint!("termdisk: one path at most\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    if summary {
        return print_summary(&path.unwrap_or_else(|| PathBuf::from(".")));
    }
    interactive(path.as_deref(), read_only)
}

/// The view in the terminal until `q`. The terminal is put back on every way out: a normal
/// end, an error, a panic on this thread, and SIGINT, SIGTERM or SIGHUP.
fn interactive(path: Option<&Path>, read_only: bool) -> ExitCode {
    // Before any thread starts, so all of them inherit the mask and only `sigwait` sees these.
    let signals = block_signals();
    let mut view = View::new(Policy::detect(read_only), path);
    let var = |k: &str| std::env::var(k).ok();
    view.set_colors(Colors::default().fitted(color_depth(&var)));
    let icons = match icon_choice(&var) {
        Ok(set) => {
            view.set_icons(set);
            None
        }
        Err(()) => Some(detect_icons(signals)),
    };
    if let Err(e) = setup() {
        restore();
        eprintln!("termdisk: cannot set up the terminal: {e}");
        return ExitCode::FAILURE;
    }
    let _ = thread::Builder::new()
        .name("termdisk-signals".into())
        .spawn(move || {
            let mut sig = 0;
            // SAFETY: `signals` is a valid set, `sig` is writable.
            unsafe { libc::sigwait(&signals, &mut sig) };
            restore();
            std::process::exit(128 + sig);
        });
    let main = thread::current().id();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if thread::current().id() == main {
            restore();
        }
        hook(info);
    }));
    let result = run(&mut view, signals, icons);
    restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("termdisk: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The loop. `icons` brings the icon set once it is known: the first frames do not wait for
/// it (they use Unicode), and the loop looks for it every `ICONS_CHECK` until it comes.
fn run(
    view: &mut View,
    signals: libc::sigset_t,
    mut icons: Option<Receiver<IconSet>>,
) -> io::Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut dirty = true;
    let started = Instant::now();
    loop {
        if let Some(rx) = &icons {
            match rx.try_recv() {
                Ok(set) => {
                    view.set_icons(set);
                    dirty |= set != IconSet::default();
                    icons = None;
                }
                Err(TryRecvError::Disconnected) => icons = None,
                Err(TryRecvError::Empty) if started.elapsed() > ICONS_WAIT => {
                    kill_probe();
                    icons = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if dirty {
            terminal.draw(|f| view.draw(f.area(), f.buffer_mut()))?;
            dirty = false;
        }
        // Nothing to wait for: block until an event (no wake-ups while idle).
        let wake = match (view.next_wake(), icons.is_some()) {
            (wake, false) => wake,
            (wake, true) => Some(wake.map_or(ICONS_CHECK, |d| d.min(ICONS_CHECK))),
        };
        let ready = match wake {
            None => true,
            Some(d) => event::poll(d)?,
        };
        if ready {
            let out = match event::read()? {
                Event::Key(k) => view.handle_key(k),
                Event::Mouse(m) => view.handle_mouse(m),
                Event::Resize(..) => Outcome::Redraw,
                _ => Outcome::None,
            };
            match out {
                Outcome::Quit => return Ok(()),
                Outcome::None => {}
                Outcome::Redraw => dirty = true,
                Outcome::Open(p) => open(&p, signals),
                Outcome::CopyPath(p) => {
                    copy(&p)?;
                    dirty = true;
                }
            }
        }
        dirty |= view.tick();
    }
}

/// Raw mode, the alternate screen, the mouse, and bracketed paste: pasted text then comes as
/// one `Event::Paste` (ignored), never as keys, so a pasted Tab and Enter cannot answer the
/// trash dialog.
fn setup() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        cursor::Hide
    )
}

/// Undoes `setup`, and kills a font probe still running; harmless to call twice. Every way
/// out comes here: a normal end, an error, a panic on the loop's thread, a signal.
fn restore() {
    kill_probe();
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
}

/// The terminal's colour depth: TERMDISK_COLORS (`truecolor`, `24bit`, `256`, `16`) if set,
/// else from the environment, as termcode does it (src/colors.rs `detect`, MIT, the same
/// author). GNU screen shows at most 256 colours whatever COLORTERM says; tmux and herdr
/// convert for the terminal outside, so they take 24-bit colour (if tmux is not set up for
/// it, colours come out wrong: TERMDISK_COLORS=256). COLORTERM does not cross ssh by default,
/// so over ssh this is usually 256 (from TERM) unless the terminal is named.
fn color_depth(var: &dyn Fn(&str) -> Option<String>) -> Depth {
    match var("TERMDISK_COLORS").as_deref() {
        Some("truecolor" | "24bit") => return Depth::True,
        Some("256") => return Depth::C256,
        Some("16") => return Depth::C16,
        _ => {}
    }
    let term = var("TERM").unwrap_or_default();
    let set = |k: &str| var(k).is_some_and(|v| !v.is_empty());
    let fallback = if term.contains("256") {
        Depth::C256
    } else {
        Depth::C16
    };
    if set("STY") && !term.starts_with("tmux") {
        return fallback;
    }
    const TERMS: [&str; 7] = [
        "kitty",
        "ghostty",
        "alacritty",
        "foot",
        "wezterm",
        "contour",
        "rio",
    ];
    const PROGRAMS: [&str; 7] = [
        "ghostty",
        "iTerm.app",
        "WezTerm",
        "vscode",
        "Hyper",
        "Tabby",
        "rio",
    ];
    let truecolor = matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit"))
        || term.ends_with("-direct")
        || set("TMUX")
        || set("HERDR_ENV")
        || TERMS.iter().any(|t| term.contains(t))
        || var("TERM_PROGRAM").is_some_and(|p| PROGRAMS.contains(&p.as_str()));
    if truecolor {
        Depth::True
    } else {
        fallback
    }
}

/// The icon set the environment decides: TERMDISK_ICONS (`nerd`, `unicode`, `ascii` or `0`),
/// else Unicode over ssh (the fonts there are the other computer's). `Err`: find out whether a
/// Nerd Font is installed (`detect_icons`).
fn icon_choice(var: &dyn Fn(&str) -> Option<String>) -> Result<IconSet, ()> {
    match var("TERMDISK_ICONS").as_deref() {
        Some("nerd") => return Ok(IconSet::Nerd),
        Some("unicode") => return Ok(IconSet::Unicode),
        Some("ascii" | "0") => return Ok(IconSet::Ascii),
        _ => {}
    }
    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];
    if ssh.iter().any(|k| var(k).is_some()) {
        return Ok(IconSet::Unicode);
    }
    Err(())
}

/// The pid of a running `fc-list` (`detect_icons`). Whoever holds the lock decides its fate:
/// `kill_probe` kills it under the lock, and the reading thread reaps it only after taking it
/// out under the lock, so a pid is never killed after it was reaped.
static PROBE: Mutex<Option<u32>> = Mutex::new(None);

/// Asks fontconfig, on a thread of its own, whether a font with the codicons (U+EA60) is
/// installed, as termcode does: Nerd if so, else Unicode. `fc-list` takes 10-15 ms here, so
/// nothing waits for it; one that hangs is killed after `ICONS_WAIT` or when termdisk ends.
fn detect_icons(signals: libc::sigset_t) -> Receiver<IconSet> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("termdisk-icons".into())
        .spawn(move || {
            let mut cmd = Command::new("fc-list");
            cmd.args([":charset=ea60", "family"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            // SAFETY: pthread_sigmask is async-signal-safe; the child gets its signals back.
            unsafe {
                cmd.pre_exec(move || {
                    libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
                    Ok(())
                });
            }
            let Ok(mut child) = cmd.spawn() else {
                let _ = tx.send(IconSet::Unicode);
                return;
            };
            *probe() = Some(child.id());
            let mut out = Vec::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_end(&mut out);
            }
            *probe() = None; // from here on nobody kills it: it is reaped next
            let ok = child.wait().is_ok_and(|s| s.success());
            let found = ok && !out.trim_ascii().is_empty();
            let _ = tx.send(if found {
                IconSet::Nerd
            } else {
                IconSet::Unicode
            });
        });
    rx
}

fn probe() -> std::sync::MutexGuard<'static, Option<u32>> {
    PROBE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Kills an `fc-list` still running, so none outlives termdisk.
fn kill_probe() {
    let mut p = probe();
    if let Some(pid) = p.take() {
        // SAFETY: a plain kill(2) of our own child, not yet reaped (see PROBE).
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

fn block_signals() -> libc::sigset_t {
    // SAFETY: plain calls on a local set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

/// Opens `path` with the desktop's default program, detached from the terminal.
fn open(path: &Path, signals: libc::sigset_t) {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pthread_sigmask is async-signal-safe; the child gets its signals back.
    unsafe {
        cmd.pre_exec(move || {
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
            Ok(())
        });
    }
    if let Ok(mut child) = cmd.spawn() {
        thread::spawn(move || child.wait());
    }
}

/// Puts `path` on the clipboard through the terminal (OSC 52).
fn copy(path: &Path) -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(path.as_os_str().as_bytes()))?;
    out.flush()
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn print_summary(path: &Path) -> ExitCode {
    let euid = unsafe { libc::geteuid() };
    let start = Instant::now();
    let tree = match termdisk::scan(path, euid) {
        Ok(scan) => scan.join(),
        Err(e) => {
            eprintln!("termdisk: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(tree) = tree else {
        eprintln!("termdisk: the scan stopped");
        return ExitCode::FAILURE;
    };
    let secs = start.elapsed().as_secs_f64();
    let r = tree.node(tree.root());
    let dirs = (1..tree.len() as NodeId)
        .filter(|&i| tree.node(i).is_dir())
        .count();
    // MOUNT marks the mount points themselves (the folders above one hold it: HOLDS); the scan
    // did not enter them, so they have no children.
    let mounts = (1..tree.len() as NodeId)
        .filter(|&i| tree.node(i).flags & MOUNT != 0 && tree.children(i).is_empty())
        .count();
    let heap = tree.heap_bytes();
    println!("path        {}", tree.root_path().display());
    println!("disk usage  {} ({} bytes)", fmt_size(r.alloc), r.alloc);
    println!(
        "apparent    {} ({} bytes)",
        fmt_size(r.apparent),
        r.apparent
    );
    println!(
        "items       {} ({dirs} folders, {mounts} mount points not entered, {} unreadable)",
        tree.items(tree.root()),
        tree.errors
    );
    println!("time        {secs:.3} s");
    println!(
        "tree        {} entries, {heap} bytes, {:.1} B/entry",
        tree.len(),
        heap as f64 / tree.len() as f64
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_the_rfc_examples() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, coded) in cases {
            assert_eq!(super::base64(plain.as_bytes()), coded);
        }
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    }

    #[test]
    fn the_environment_decides_the_colour_depth_and_the_variable_wins() {
        use super::color_depth;
        use termdisk::Depth::*;
        let cases: [(&[(&str, &str)], _); 8] = [
            (
                &[("COLORTERM", "truecolor"), ("TERM", "xterm-256color")],
                True,
            ),
            (&[("TERM", "xterm-256color")], C256),
            (&[("TERM", "xterm")], C16),
            (&[("TERM", "xterm-ghostty")], True),
            (
                &[
                    ("TERM", "tmux-256color"),
                    ("TMUX", "/tmp/tmux-1000/default,1,0"),
                ],
                True,
            ),
            // GNU screen keeps the outer COLORTERM but shows 256 at most.
            (
                &[
                    ("COLORTERM", "truecolor"),
                    ("TERM", "screen.xterm-256color"),
                    ("STY", "1.pts"),
                ],
                C256,
            ),
            (
                &[("COLORTERM", "truecolor"), ("TERMDISK_COLORS", "16")],
                C16,
            ),
            (&[("TERM", "xterm"), ("TERMDISK_COLORS", "truecolor")], True),
        ];
        for (vars, want) in cases {
            assert_eq!(color_depth(&env(vars)), want, "{vars:?}");
        }
    }

    #[test]
    fn the_variable_or_ssh_decides_the_icons_else_fontconfig_is_asked() {
        use super::icon_choice;
        use termdisk::IconSet::*;
        assert_eq!(icon_choice(&env(&[("TERMDISK_ICONS", "ascii")])), Ok(Ascii));
        assert_eq!(icon_choice(&env(&[("TERMDISK_ICONS", "0")])), Ok(Ascii));
        assert_eq!(
            icon_choice(&env(&[
                ("TERMDISK_ICONS", "nerd"),
                ("SSH_TTY", "/dev/pts/3")
            ])),
            Ok(Nerd)
        );
        assert_eq!(
            icon_choice(&env(&[("SSH_CONNECTION", "1 2 3 4")])),
            Ok(Unicode)
        );
        assert_eq!(icon_choice(&env(&[])), Err(()));
    }
}
