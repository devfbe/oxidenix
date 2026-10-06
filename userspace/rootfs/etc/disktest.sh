# Exercises the ext2 driver on /data; every line should end in "ok".
set -u
D=/data/disktest
rm -rf $D
free_at_start=$(df /data | tail -1 | awk '{print $4}')
mkdir $D || exit 1
check() { if [ "$2" = "$3" ]; then echo "$1: ok"; else echo "$1: FAIL ($2 != $3)"; fi; }

for i in $(seq 1 150); do echo "file $i" > $D/f$i; done
check "150 files (directory grows beyond one block)" "$(ls $D | wc -l)" 150
check "file content" "$(cat $D/f77)" "file 77"

dd if=/dev/zero bs=1024 count=1500 2>/dev/null | tr '\0' 'x' > $D/big
check "1.5 MiB file (double indirect blocks)" "$(wc -c < $D/big)" 1536000
check "big file content" "$(tr -d 'x' < $D/big | wc -c)" 0

head -c 5000 $D/big > $D/small; cat $D/small >> $D/small2; echo tail >> $D/small2
check "append" "$(wc -c < $D/small2)" 5005
truncate -s 100 $D/big
check "truncate shrinks" "$(wc -c < $D/big)" 100
truncate -s 3000 $D/big
check "truncate grows with zeros" "$(tr -d '\0x' < $D/big | wc -c)" 0

mkdir -p $D/a/b/c; echo deep > $D/a/b/c/file
mv $D/a $D/moved
check "rename a directory" "$(cat $D/moved/b/c/file)" deep
mv $D/moved $D/moved/b/c 2>/dev/null
check "no directory below itself" "$?" 1
mv $D/f1 $D/f2
check "rename over a file" "$(cat $D/f2)" "file 1"
rmdir $D/moved 2>/dev/null
check "rmdir refuses non-empty" "$?" 1

ln -s /data/./disktest/./moved/./b/./c/../../../../disktest/./././././f2 $D/longlink
check "long symlink (stored in a block)" "$(cat $D/longlink)" "file 1"
ln -s f2 $D/shortlink
check "short symlink" "$(readlink $D/shortlink)" f2

rm -r $D
check "rm -r removes everything" "$(ls /data | grep -c disktest)" 0
check "all space is returned" "$(df /data | tail -1 | awk '{print $4}')" "$free_at_start"
echo "disktest done"
