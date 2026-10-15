# Runs every self-test and exits non-zero if any fails. In test mode
# (OXIDENIX_TEST=1) /etc/autorun points here and the kernel turns the exit
# status into QEMU's.
failed=0
fail() { echo "FAIL $1"; failed=$((failed + 1)); }

for t in forktest waittest pidfdtest leaktest sigtest sigframetest jobtest cowtest fstest oomtest nomemtest nettest smptest proctest vmtest futextest pitest threadtest timetest vdsotest randtest timertest cputimertest posixtimertest timerfdtest fdtest polltest eventfdtest sigmasktest epolltest unixtest mmaptest exectest cachetest reclaimtest writebacktest datatest ttytest lxtest libuvtest metatest inotifytest kmsgtest treetest fifotest oomkilltest membarriertest; do
    echo "=== $t"
    if $t; then echo "PASS $t"; else fail "$t"; fi
done

# lxtest again, three times, its output into a pipe and beside a busy
# reader of /proc (procfs over its channel, the server's own files) that
# forks, execs and waits all the time: its checks hold in any run and
# whatever else runs.
echo "=== lxtest x3 beside a busy reader"
(while :; do cat /proc/counters /proc/self/stat > /dev/null; done) &
busy=$!
runs=0
for i in 1 2 3; do
    if lxtest | tee /tmp/lxtest.out | grep -v ' ok$'; [ "$(tail -n 1 /tmp/lxtest.out)" = "lxtest: all passed" ]; then
        runs=$((runs + 1))
    fi
done
kill $busy
wait $busy 2>/dev/null
rm -f /tmp/lxtest.out
if [ $runs -eq 3 ]; then echo "PASS lxtest x3"; else fail "lxtest x3 ($runs of 3 passed)"; fi

# Dynamically linked programs as Nix built them (ADR 0021): their interpreters
# (glibc's ld-linux-x86-64.so.2, musl's ld-musl-x86_64.so.1) and libraries come
# from the data disk's /nix/store (/nix is a link to /data/nix).
for t in dyntest dynmusl dyncxx; do
    echo "=== $t"
    if $t; then echo "PASS $t"; else fail "$t"; fi
done
echo "=== gnu-hello"
out=$(gnu-hello 2>&1)
if [ "$out" = "Hello, world!" ]; then echo "PASS nixpkgs' GNU hello runs"; else fail "gnu-hello: '$out'"; fi
# nixpkgs' coreutils as shipped (glibc, libacl, libattr, libgmp): one binary
# that is each tool by the name it runs as, in /usr/gnu/bin (a link into the
# store). Each command's output against BusyBox's or the expected, and nothing
# on stderr (df reads /proc/self/mountinfo, ls asks for ACLs with lgetxattr).
echo "=== GNU coreutils"
G=/usr/gnu/bin
gnu() {
    name=$1; want=$2; shift 2
    got=$("$@" 2>/tmp/gnu.err)
    if [ "$got" = "$want" ] && [ ! -s /tmp/gnu.err ]; then echo "PASS coreutils $name"; else fail "coreutils $name: '$got' (stderr: $(cat /tmp/gnu.err))"; fi
}
gnu "ls -d" /etc $G/ls -d /etc
gnu "ls -l" "$(ls -l /etc/passwd | sed 's/  */ /g' | cut -d' ' -f1,5)" sh -c "$G/ls -l /etc/passwd | $G/tr -s ' ' | $G/cut -d' ' -f1,5"
gnu "ls -la /data" ok sh -c "$G/ls -la /data > /dev/null && echo ok"
gnu sort "3 2 1" sh -c "$G/seq 1 3 | $G/sort -rn | $G/tr '\n' ' ' | sed 's/ \$//'"
gnu sha256sum "$(sha256sum /etc/passwd)" $G/sha256sum /etc/passwd
gnu stat "$(stat -c '%s %i %F' /etc/passwd)" $G/stat -c '%s %i %F' /etc/passwd
gnu "df /data" "/data" sh -c "$G/df /data | $G/tail -n 1 | $G/tr -s ' ' | $G/cut -d' ' -f6"
gnu "date -d @0" 1970-01-01 $G/date -u -d @0 +%F
gnu "cp, mv, cat, rm on /data" "$(cat /etc/passwd)" sh -c "$G/cp /etc/passwd /data/gnu.cp && $G/mv /data/gnu.cp /data/gnu.mv && $G/cat /data/gnu.mv && $G/rm /data/gnu.mv"
gnu "timeout" 124 sh -c "$G/timeout 0.2 $G/sleep 5; echo \$?"
gnu "id" "uid=0(root) gid=0(root) groups=0(root)" $G/id
rm -f /tmp/gnu.err

echo "=== mmaptest /data"
if mmaptest /data; then echo "PASS mmaptest /data"; else fail "mmaptest /data"; fi

echo "=== disktest"
out=$(sh /etc/disktest.sh 2>&1)
echo "$out"
if echo "$out" | grep -q FAIL || [ "$(echo "$out" | grep -c ': ok')" -ne 15 ]; then fail disktest; else echo "PASS disktest"; fi

echo "=== bash"
out=$(bash -c '
    cd /
    f() { echo "f:$1"; }
    a=(x y z); n=$(( ${#a[@]} * 7 ))
    f "${a[1]}"; echo "n=$n"
    [[ "abc" == a* ]] && echo glob
    printf "%s\n" one two three | grep -c o
    cat <<END > /tmp/bash.$$
here $n
END
    read -r line < /tmp/bash.$$; echo "$line"; rm /tmp/bash.$$
    (cd /etc && pwd); pwd
    echo "$(basename /bin/sh)"
' 2>&1)
echo "$out"
want="f:y
n=21
glob
2
here 21
/etc
/
sh"
if [ "$out" = "$want" ]; then echo "PASS bash"; else fail bash; fi

echo "=== test.sh"
out=$(sh /etc/test.sh 2>&1)
echo "$out"
for want in "hello file" "Invalid argument" "No space left on device" "5000"; do
    case "$out" in
        *"$want"*) ;;
        *) fail "test.sh: missing '$want'" ;;
    esac
done
echo "PASS test.sh checks done"

echo "=== vfork (busybox timeout)"
timeout 1 sleep 5
if [ $? -eq 143 ]; then echo "PASS timeout stops a command"; else fail "timeout"; fi

echo "=== uname"
if [ "$(uname -s)" = oxidenix ]; then echo "PASS uname names the system oxidenix"; else fail "uname -s: $(uname -s)"; fi

echo "=== pid 1"
# The servers have no pid in a tree's namespace (no program can signal them); the tree's
# pid 1 (this script) takes no signal it has no handler for from inside the tree, as a pid
# namespace's init on Linux.
kill -9 1
echo "PASS pid 1 survives SIGKILL from its own tree"

# The restart policy (ADR 0006) on the test service (ringtest), dying at
# every use.
echo "=== lxtest crashloop"
if lxtest crashloop; then echo "PASS lxtest crashloop"; else fail "lxtest crashloop"; fi

# The builder boots once more with this disk read-only (/etc/rotests.sh), which
# reads this file back, and uses a FIFO and a device node (BusyBox's mkfifo
# and mknod) there.
echo "=== a file, a FIFO and a device node for the read-only boot"
if mkdir -p /data/rotest && echo "written by the self-tests" > /data/rotest/written &&
    mkfifo -m 600 /data/rotest/fifo && mknod -m 666 /data/rotest/null c 1 3 && sync; then
    echo "PASS /data/rotest/written, fifo, null"
else
    fail "/data/rotest/written, fifo, null"
fi

echo "=== summary: $failed failed"
[ "$failed" -eq 0 ]
