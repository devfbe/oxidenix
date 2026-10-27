# The self-tests of USB tethering: OXIDENIX_TEST=1 boots a third time on a
# machine without a network card, with USB Ethernet functions on its xHCI
# controller instead (QEMU's usb-net: CDC-ECM on a root port, RNDIS behind a
# USB hub; builder/src/main.rs, USB_NETWORK), OXIDENIX_TEST=usb boots so alone;
# /etc/autorun runs this script. Exits non-zero if any check fails.
failed=0
fail() { echo "FAIL $1"; failed=$((failed + 1)); }

echo "=== usbnettest"
if usbnettest; then echo "PASS usbnettest"; else fail usbnettest; fi

# What the tools a user would reach for say (BusyBox's ip, over rtnetlink).
echo "=== ip"
links=$(ip -o link 2>&1)
echo "$links"
case "$links" in *usb0*usb1*) echo "PASS ip link: usb0, usb1" ;; *) fail "ip link: '$links'" ;; esac
routes=$(ip route 2>&1)
echo "$routes"
case "$routes" in *"default via 10.0.3.2 dev usb0"*) echo "PASS the default route through usb0" ;; *) fail "ip route: '$routes'" ;; esac
case "$routes" in *"10.0.4.0/24 dev usb1"*) echo "PASS usb1's network through usb1" ;; *) fail "ip route: no route to usb1's network" ;; esac

# Name lookups go to the DNS server of the default link's lease (QEMU's at
# 10.0.3.3 on usb0's network; a phone's own address when tethering):
# /etc/resolv.conf is a link to /proc/net/pnp, the leases' DNS servers.
echo "=== resolv.conf"
conf=$(cat /etc/resolv.conf 2>&1)
echo "$conf"
case "$conf" in *"nameserver 10.0.3.3"*"nameserver 10.0.4.3"*) echo "PASS usb0's DNS server first, then usb1's" ;; *) fail "resolv.conf: '$conf'" ;; esac

# The kernel log says what usbd found.
echo "=== dmesg"
log=$(dmesg | grep -E 'usbd|netd: usb')
echo "$log"
case "$log" in *"cdc_ether 52:54:00:aa:00:01"*) echo "PASS usbd: ECM on the root port" ;; *) fail "usbd: no ECM function" ;; esac
case "$log" in *"hub with 8 ports"*) echo "PASS usbd: the hub" ;; *) fail "usbd: no hub" ;; esac
case "$log" in *"rndis_host 52:54:00:aa:00:02"*) echo "PASS usbd: RNDIS behind the hub" ;; *) fail "usbd: no RNDIS function" ;; esac

echo "=== summary: $failed failed"
[ "$failed" -eq 0 ]
