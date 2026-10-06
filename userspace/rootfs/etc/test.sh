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
