//! The termdisk CLI: the view in the terminal, or `--summary`. All terminal code lives here.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Instant;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::Terminal;
use termdisk::tree::MOUNT;
use termdisk::{fmt_size, NodeId, Outcome, Policy, View};

const USAGE: &str = "usage: termdisk [--read-only] [PATH]
       termdisk --summary [PATH]

  PATH          scan PATH at once (without it: choose home, cache, /tmp or the trash)
  --read-only   look only: nothing can be moved to the trash
  --summary     scan PATH (default: the current folder), print its totals and exit
";

fn main() -> ExitCode {
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
    let result = run(&mut view, signals);
    restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("termdisk: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(view: &mut View, signals: libc::sigset_t) -> io::Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| view.draw(f.area(), f.buffer_mut()))?;
            dirty = false;
        }
        // Nothing to wait for: block until an event (no wake-ups while idle).
        let ready = match view.next_wake() {
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

/// Undoes `setup`; harmless to call twice.
fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
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
    // A mount point is a MOUNT folder the scan did not enter; its ancestors have children.
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
        r.items, tree.errors
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
}
