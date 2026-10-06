echo "== ls /"
ls /
echo "== cat /etc/motd"
cat /etc/motd
echo "== write and read a file"
echo "hello file" > /tmp/x
cat /tmp/x
echo "== Pipes"
ls /bin | wc -l
echo eins zwei drei | wc -w
echo "== cd and pwd"
cd /etc && pwd
echo "== mkdir, touch, ls -l, rm"
mkdir /tmp/d && touch /tmp/d/f && ls -l /tmp/d && rm -r /tmp/d && ls /tmp
uname -a
echo "== moving a directory below itself via a symlink (must fail)"
mkdir -p /tmp/a/b && ln -s /tmp/a/b /tmp/link && mv /tmp/a /tmp/link/a
ls /tmp
rm -r /tmp/a /tmp/link
echo "== exceeding the file quota (must fail with no space)"
dd if=/dev/zero of=/tmp/big bs=1048576 count=20
rm /tmp/big
dd if=/dev/zero of=/tmp/big bs=1048576 count=2 && ls -l /tmp/big && rm /tmp/big
