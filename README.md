# vmmbox

Accelerated Linux VMs (Ubuntu, Debian, Fedora) on QEMU, with your user and `$HOME`.

```
brew install ericcurtin/tap/vmmbox                                   # macOS, Linux
scoop bucket add vmmbox https://github.com/ericcurtin/scoop-bucket   # Windows
scoop install vmmbox
```

```
vmmbox run ubuntu bash    # pulls, creates and starts the VM as needed
                          # (or ubuntu:24.04, fedora:44)
vmmbox ls                 # also: ps, images, pull, start, stop, rm, rmi
```

Apache-2.0, see [LICENSE](LICENSE).
