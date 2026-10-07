# Runs every self-test and exits non-zero if any fails. In test mode
# (OXIDENIX_TEST=1) /etc/autorun points here and the kernel turns the exit
# status into QEMU's.
failed=0
fail() { echo "FAIL $1"; failed=$((failed + 1)); }

for t in forktest sigtest jobtest cowtest fstest oomtest nettest smptest proctest vmtest futextest threadtest timetest timertest polltest eventfdtest sigmasktest epolltest mmaptest exectest; do
    echo "=== $t"
    if $t; then echo "PASS $t"; else fail "$t"; fi
done

echo "=== disktest"
out=$(sh /etc/disktest.sh 2>&1)
echo "$out"
if echo "$out" | grep -q FAIL || [ "$(echo "$out" | grep -c ': ok')" -ne 15 ]; then fail disktest; else echo "PASS disktest"; fi

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

echo "=== server protection"
if kill -9 1 2>/dev/null; then fail "a server could be killed from user space"; else echo "PASS servers are protected"; fi

echo "=== summary: $failed failed"
[ "$failed" -eq 0 ]
