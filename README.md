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

## GPU

On Linux hosts a guest gets OpenGL and Vulkan through the host's render node.
`vmmbox run <distro>` prints the `GPU:` line, and `VMMBOX_GPU=none` turns it off.

On a Mac with macOS 26 or newer, vmmbox can give a guest Vulkan on Metal (the
guest's Mesa Venus driver, virglrenderer in QEMU, and KosmicKrisp), enough to run
llama.cpp's Vulkan backend. It is used when the QEMU that vmmbox runs has a
Venus-capable `virtio-gpu-gl-pci` and a KosmicKrisp driver beside it. The QEMU
vmmbox ships does not yet: stock QEMU needs an OpenGL display for that device,
which macOS lacks, so this waits for QEMU and virglrenderer to support it.

The guest has to cooperate too. Metal maps GPU memory in 16 KiB pages, so the
guest must place it that way, which needs Linux 7.2 and Mesa 26.3. vmmbox warns
after boot if the guest kernel is older than 7.2.

Apache-2.0, see [LICENSE](LICENSE).
