# Security

## Report a vulnerability privately

Use GitHub's private reporting: the repository's **Security** tab, **Report a vulnerability**
([direct link](https://github.com/JonasAtelier/termdisk/security/advisories/new)). Only you and
the maintainer see the report. Please never report a vulnerability in a public issue, pull
request or discussion: anyone could use it before a fix is out.

Say what happens, on which termdisk version (`termdisk --version`), distribution, filesystem
and kernel, and how to make it happen again. A folder layout made with a short script is the
most useful proof.

## Supported versions

Only the latest release gets fixes. A fix comes as a new release, on crates.io and GitHub.

## In scope

termdisk moves files to the trash and empties the trash, so what matters most is any way it
could do that to the wrong file. In particular:

- moving or deleting something it should not: through a symbolic link, across a mount point
  (bind mounts too) or onto another filesystem, by a race between its checks and the move or
  the delete, in spite of permissions or ownership, or inside a protected path (the lists at
  the top of `src/guard.rs`: the home itself, `~/.config`, `~/.local`, `~/.ssh` and the rest);
- emptying the trash reaching anything beyond the home trash's `files` and `info`;
- reading outside where it should: a scan that leaves its folder or its filesystem, follows a
  symbolic link, or shows what the user may not read;
- the release workflow letting anyone but the maintainer publish termdisk.

A bug that only loses termdisk's own view (a wrong size, a crash) is an ordinary issue.

## What to expect

termdisk has one maintainer, working on it in their own time, so there are no fixed deadlines.
Each report is read, answered when it has been looked at, and kept private while a fix is
made. A confirmed vulnerability is fixed in a release as soon as it can be, with a GitHub
security advisory that credits you, unless you would rather not be named. Please keep it to
yourself until that release is out. No bounty is offered.
