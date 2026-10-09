# Runs the Node.js smoke tests (userspace/node/tests, in the root
# filesystem as /usr/lib/node-tests) with /data/bin/node and exits non-zero
# if any fails. From kernel/:
#   OXIDENIX_NODE=1 OXIDENIX_AUTORUN=$PWD/../userspace/node/run-node.sh cargo run
# Arguments (as NODE_TESTS="fs os" in the environment) pick tests by name.
node=/data/bin/node
cd /usr/lib/node-tests || exit 2
[ -x "$node" ] || { echo "no $node (OXIDENIX_NODE=1 installs it)"; exit 2; }
"$node" --version

failed=0
for t in ${NODE_TESTS:-$(ls *.test.mjs *.test.cjs 2>/dev/null | sed 's/\.test\.[cm]js$//')}; do
    f=$(ls "$t".test.* 2>/dev/null | head -n 1)
    echo "=== $t"
    case "$t" in
        # readline reads these lines from its standard input.
        readline) printf 'first line\nsecond line\nthird\n' | timeout 300 "$node" "$f" ;;
        *) timeout 300 "$node" "$f" ;;
    esac
    status=$?
    if [ $status -eq 0 ]; then echo "PASS $t"; else echo "FAIL $t (exit $status)"; failed=$((failed + 1)); fi
done
echo "node tests: $failed failed"
[ $failed -eq 0 ]
