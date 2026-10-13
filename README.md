# vmmbox

Accelerated Linux VMs (Ubuntu, Debian, Fedora) on QEMU, with your user and `$HOME`.

```
brew install ericcurtin/tap/vmmbox                                   # macOS, Linux
scoop bucket add vmmbox https://github.com/ericcurtin/scoop-bucket   # Windows
scoop install vmmbox
```

Supported hosts: macOS on Apple silicon, Linux and Windows on x86_64 or aarch64.

```
vmmbox run ubuntu bash    # pulls, creates and starts the VM as needed
                          # (or ubuntu:24.04, fedora:44)
vmmbox run fedora         # no command: just start the VM
vmmbox ls                 # also: ps, images, pull, stop, rm, rmi
```

Apache-2.0, see [LICENSE](LICENSE).
