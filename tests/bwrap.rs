//! `termdisk --summary` on real mounts, made inside a bwrap sandbox (a user and mount
//! namespace: nothing outside it changes): a tmpfs, and a bind mount of a folder from the same
//! filesystem, under a root given as a relative path. Skipped where bwrap cannot run.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn mounts_under_a_relative_root_are_listed_not_entered() {
    let t = std::env::temp_dir().join(format!("termdisk-cli-bwrap-{}", std::process::id()));
    for d in ["root/tm", "root/bind", "src"] {
        fs::create_dir_all(t.join(d)).unwrap();
    }
    fs::write(t.join("src/big"), vec![0u8; 1 << 20]).unwrap();
    fs::write(t.join("root/small"), b"x").unwrap();
    let run = |cmd: &[&str]| {
        Command::new("bwrap")
            .args(["--dev-bind", "/", "/", "--tmpfs"])
            .arg(t.join("root/tm"))
            .arg("--bind")
            .args([t.join("src"), t.join("root/bind")])
            .arg("--chdir")
            .arg(&t)
            .arg("--")
            .args(cmd)
            .output()
    };
    let usable = run(&["true"]).is_ok_and(|o| o.status.success());
    let out = usable.then(|| run(&[env!("CARGO_BIN_EXE_termdisk"), "--summary", "root"]));
    let _ = fs::remove_dir_all(&t); // test-only cleanup
    let Some(out) = out else {
        eprintln!("skipped: bwrap is missing or cannot make a sandbox here");
        return;
    };
    let out = out.unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .unwrap()
            .to_string()
    };
    assert_eq!(
        line("path"),
        format!("path        {}", fs_path(&t).join("root").display())
    );
    assert!(
        line("items").contains("2 mount points not entered"),
        "{text}"
    );
    let bytes: u64 = line("apparent")
        .split(['(', ' '])
        .find_map(|w| w.parse().ok())
        .unwrap();
    assert!(
        bytes < 1 << 20,
        "the 1 MiB file behind the bind mount is not counted: {text}"
    );
}

/// `t` as the scan shows it: canonical (the temp folder may be reached through a symlink).
fn fs_path(t: &std::path::Path) -> PathBuf {
    fs::canonicalize(t.parent().unwrap())
        .unwrap()
        .join(t.file_name().unwrap())
}
