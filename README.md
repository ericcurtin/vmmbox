# vmmbox

Accelerated Linux VMs (Ubuntu, Debian, Fedora) on QEMU, with your user and `$HOME`.

```
vmmbox pull ubuntu        # or ubuntu:24.04, fedora:44
vmmbox start ubuntu
vmmbox exec ubuntu bash
vmmbox ps                 # also: images, stop
```

Needs `qemu`, `ssh` and `curl`. GPL-2.0, see [LICENSE](LICENSE).
