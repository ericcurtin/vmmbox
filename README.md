# vmmbox

Accelerated Linux VMs (Ubuntu, Debian, Fedora) on QEMU, with your user and `$HOME`.

```
brew install ericcurtin/tap/vmmbox                                   # macOS, Linux
scoop bucket add vmmbox https://github.com/ericcurtin/scoop-bucket   # Windows
scoop install vmmbox
```

```
vmmbox pull ubuntu        # or ubuntu:24.04, fedora:44
vmmbox start ubuntu
vmmbox exec ubuntu bash
vmmbox ps                 # also: images, stop, rm, rmi
```

GPL-2.0, see [LICENSE](LICENSE).
