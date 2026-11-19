#!/usr/bin/env python3
# Drives termdisk in a pty (120x40) with its own temporary HOME, XDG folders and a fixture, and
# reports what a person cannot see from outside.
#
# Usage: scripts/smoke.py BINARY STEPS [--read-only] [PATH]
#   PATH defaults to the fixture's ~/.cache. A PATH outside the temporary folder is refused
#   unless --read-only is given (then termdisk can move nothing to any trash). The fixture's
#   ~/.cache holds big/ (20 folders of 500 files) and a file, small.
# STEPS, comma-separated:
#   sleep:S       wait S seconds
#   key:X         type X (bash $'key:\x1b[3~' is Del, $'key:\t' Tab, $'key:\r' Enter)
#   paste:X       paste X as a terminal with bracketed paste does (\e[200~ X \e[201~)
#   rawpaste:X    paste X as a terminal without it does: all of X in one write
#   mark          start the clock again for the next waitfor
#   waitfor:TEXT  wait (60 s at most) for TEXT in the output since launch or the last mark;
#                 print how long it took (only for whole screens: a redraw writes changed
#                 cells, so text that replaces other text may come in pieces)
#   waitout       wait (60 s at most) for any output after the last mark; print how long
#   threads       print its thread count
#   ctx:S         print its voluntary context switches (wake-ups) over S seconds
#   winsize:C;R   resize the terminal to C columns, R rows
#   signal:NAME   send it a signal (TERM, INT, HUP)
# At the end it waits for termdisk to exit (40 s at most) and prints the exit code, its max RSS,
# whether the terminal was put back, and what the temporary trash holds; then removes it all.
import fcntl, os, shutil, signal, struct, subprocess, sys, tempfile, termios, threading, time

args = sys.argv[1:]
read_only = '--read-only' in args
args = [a for a in args if a != '--read-only']
if len(args) not in (2, 3):
    sys.exit(__doc__ or 'usage: scripts/smoke.py BINARY STEPS [--read-only] [PATH]')
binary, steps = os.path.abspath(args[0]), args[1].split(',')
tmp = os.path.realpath(tempfile.mkdtemp(prefix='termdisk-smoke-'))
home = os.path.join(tmp, 'home')
for d in ('.cache/big', '.local/share', '.config', 'Downloads'):
    os.makedirs(os.path.join(home, d))
os.makedirs(os.path.join(tmp, 'run'), mode=0o700)
for i in range(20):
    d = os.path.join(home, '.cache/big', f'd{i}')
    os.mkdir(d)
    for j in range(500):
        with open(os.path.join(d, f'f{j}'), 'wb') as f:
            f.write(b'x' * 100)
with open(os.path.join(home, '.cache/small'), 'wb') as f:
    f.write(b'y' * 3000)
path = os.path.realpath(args[2]) if len(args) == 3 else os.path.join(home, '.cache')
if not read_only and os.path.commonpath([path, tmp]) != tmp:
    shutil.rmtree(tmp)
    sys.exit(f'refused: {path} is outside {tmp}; give --read-only to look at it')

env = {**os.environ, 'TERM': 'xterm-256color', 'HOME': home,
       'XDG_DATA_HOME': os.path.join(home, '.local/share'),
       'XDG_CONFIG_HOME': os.path.join(home, '.config'),
       'XDG_CACHE_HOME': os.path.join(home, '.cache'),
       'XDG_RUNTIME_DIR': os.path.join(tmp, 'run')}
m, s = os.openpty()
fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 1200, 800))


def ctty():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


argv = [binary] + (['--read-only'] if read_only else []) + [path]
t0 = time.time()
p = subprocess.Popen(argv, stdin=s, stdout=s, stderr=s, preexec_fn=ctty, env=env)
os.close(s)
out = bytearray()


def drain():
    while True:
        try:
            b = os.read(m, 65536)
        except OSError:
            return
        if not b:
            return
        out.extend(b)


threading.Thread(target=drain, daemon=True).start()


def ctx():
    n = 0
    for t in os.listdir(f'/proc/{p.pid}/task'):
        for line in open(f'/proc/{p.pid}/task/{t}/status'):
            if line.startswith('voluntary_ctxt_switches'):
                n += int(line.split()[1])
    return n


clock, since = t0, 0
for st in steps:
    k, _, v = st.partition(':')
    if k == 'sleep':
        time.sleep(float(v))
    elif k in ('key', 'rawpaste'):
        os.write(m, v.encode())
    elif k == 'paste':
        os.write(m, b'\x1b[200~' + v.encode() + b'\x1b[201~')
    elif k == 'mark':
        clock, since = time.time(), len(out)
    elif k == 'waitfor':
        while v.encode() not in out[since:] and time.time() - clock < 60:
            time.sleep(0.005)
        found = v.encode() in out[since:]
        print(f'waitfor {v!r}: {"%.3f s" % (time.time() - clock) if found else "NOT SEEN in 60 s"}')
    elif k == 'waitout':
        while len(out) == since and time.time() - clock < 60:
            time.sleep(0.001)
        print(f'first output after the mark: {time.time() - clock:.3f} s')
    elif k == 'threads':
        print(f'threads: {len(os.listdir(f"/proc/{p.pid}/task"))}')
    elif k == 'ctx':
        a = ctx()
        time.sleep(float(v))
        print(f'wake-ups in {v} s: {ctx() - a}')
    elif k == 'winsize':
        c, r = (int(x) for x in v.split(';'))
        fcntl.ioctl(m, termios.TIOCSWINSZ, struct.pack('HHHH', r, c, c * 10, r * 20))
        os.killpg(p.pid, signal.SIGWINCH)
    elif k == 'signal':
        os.kill(p.pid, getattr(signal, 'SIG' + v))

rusage, status = None, None
end = time.time() + 40
while time.time() < end:
    pid, status, rusage = os.wait4(p.pid, os.WNOHANG)
    if pid:
        break
    time.sleep(0.02)
else:
    p.kill()
    pid, status, rusage = os.wait4(p.pid, 0)
    print('did not exit: killed')
time.sleep(0.2)  # the last output
code = os.waitstatus_to_exitcode(status)
print(f'exit code: {code}')
print(f'max RSS: {rusage.ru_maxrss // 1024} MB')
print('restored: alternate screen left', b'\x1b[?1049l' in out, '| cursor shown', b'\x1b[?25h' in out,
      '| mouse off', b'\x1b[?1000l' in out)
trash = os.path.join(home, '.local/share/Trash')
for sub in ('files', 'info'):
    d = os.path.join(trash, sub)
    print(f'trash {sub}:', sorted(os.listdir(d)) if os.path.isdir(d) else 'none')
info = os.path.join(trash, 'info/big.trashinfo')
if os.path.exists(info):
    print(open(info).read().strip().replace('\n', ' | '))
shutil.rmtree(tmp)
