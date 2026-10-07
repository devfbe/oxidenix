# A minimal Linux guest for comparing benchmarks (scripts/bench-linux.sh):
# the nixpkgs kernel and an initrd with BusyBox, the virtio and ext2 modules
# and the same static iobench, whose init mounts the data disk at /data,
# configures the network by DHCP and runs the benchmarks.
{ pkgs ? import <nixpkgs> { } }:
let
  kernel = pkgs.linuxPackages.kernel;
  modules = pkgs.makeModulesClosure {
    kernel = kernel.modules;
    firmware = kernel.modules;
    rootModules = [ "virtio_pci" "virtio_blk" "virtio_net" "ext2" "af_packet" ];
  };
  busybox = pkgs.pkgsStatic.busybox;
  kmod = pkgs.pkgsStatic.kmod;
  iobench = pkgs.pkgsStatic.stdenv.mkDerivation {
    name = "iobench";
    dontUnpack = true;
    buildPhase = "$CC -static -O2 -o iobench ${../userspace/iobench.c}";
    installPhase = "install -D iobench $out/bin/iobench";
  };
  dhcp = pkgs.writeScript "udhcpc.sh" ''
    #!/bin/sh
    [ "$1" = bound ] || exit 0
    ifconfig "$interface" "$ip" netmask "$subnet" up
    [ -n "$router" ] && route add default gw "$router"
    exit 0
  '';
  init = pkgs.writeScript "init" ''
    #!/bin/sh
    export PATH=/bin:/kmod/bin
    mkdir -p /proc /sys /dev /data /tmp
    mount -t proc proc /proc
    mount -t sysfs sys /sys
    mount -t devtmpfs dev /dev
    for m in virtio_pci virtio_blk virtio_net ext2 af_packet; do modprobe "$m"; done
    mount -t ext2 /dev/vda /data
    ifconfig lo 127.0.0.1 up
    ifconfig eth0 up
    udhcpc -i eth0 -q -n -t 10 -s /udhcpc.sh >/dev/null 2>&1
    echo "=== iobench"
    /iobench /data
    sync
    poweroff -f
  '';
in
{
  kernel = "${kernel}/bzImage";
  initrd = pkgs.makeInitrd {
    contents = [
      { object = init; symlink = "/init"; }
      { object = dhcp; symlink = "/udhcpc.sh"; }
      { object = "${busybox}/bin"; symlink = "/bin"; }
      { object = kmod; symlink = "/kmod"; }
      { object = "${modules}/lib"; symlink = "/lib"; }
      { object = "${iobench}/bin/iobench"; symlink = "/iobench"; }
    ];
  };
}
