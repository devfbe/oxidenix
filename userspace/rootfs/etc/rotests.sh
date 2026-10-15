# The self-tests on a read-only data disk: OXIDENIX_TEST=1 boots a second time
# with the disk the first boot wrote (QEMU's readonly=on, which virtio-blk
# reports as VIRTIO_BLK_F_RO), OXIDENIX_TEST=readonly boots so alone on a fresh
# disk; /etc/autorun runs this script ("after-rw": the first boot's last file
# is there to read). Exits non-zero if any check fails.
failed=0
fail() { echo "FAIL $1"; failed=$((failed + 1)); }

echo "=== rotest"
if rotest "$@"; then echo "PASS rotest"; else fail rotest; fi

# What BusyBox's tools say about a read-only /data (each with a time limit and
# no terminal to ask: rm would ask before removing a file it cannot write).
echo "=== tools"
out=$(timeout 10 touch /data/rotest/new 2>&1 < /dev/null)
case "$out" in *"Read-only file system"*) echo "PASS touch: $out" ;; *) fail "touch: '$out'" ;; esac
out=$(timeout 10 rm -f /data/rotest/hello 2>&1 < /dev/null)
case "$out" in *"Read-only file system"*) echo "PASS rm: $out" ;; *) fail "rm: '$out'" ;; esac
if grep -q '^/dev/vda /data ext2 ro ' /proc/mounts; then echo "PASS /proc/mounts says ro"; else fail "/proc/mounts: $(grep /data /proc/mounts)"; fi
if timeout 10 df /data > /dev/null; then echo "PASS df /data"; else fail "df /data"; fi
if timeout 10 cmp /data/rotest/hello /bin/hello; then echo "PASS cmp"; else fail cmp; fi
out=$(timeout 10 /data/rotest/hello one two)
status=$?
if [ $status -eq 42 ] && [ "$out" = "Hello from user space! argc=3 argv[0]=/data/rotest/hello" ]; then
    echo "PASS a program from /data runs"
else
    fail "/data/rotest/hello: $status '$out'"
fi
if timeout 10 sync; then echo "PASS sync"; else fail sync; fi
# Dynamically linked programs with their interpreter and libraries on the
# read-only disk (/nix/store, ADR 0021).
for t in dyntest dynmusl; do
    if out=$(timeout 60 $t 2>&1); then echo "PASS $t from a read-only store"; else echo "$out"; fail "$t from a read-only store"; fi
done

echo "=== summary: $failed failed"
[ "$failed" -eq 0 ]
