# Third-party code

Code taken from other projects is listed here with its copyright notice and
license, as those licenses require.

## libkrun: the virtio-fs server in `src/virtiofs`

These files are copied from [libkrun](https://github.com/containers/libkrun),
commit `493d707fa82fa2480e051644fc1ca6a2f1b943ae`, directory
`src/devices/src/virtio/`, and modified for vmmbox:

| vmmbox file                      | libkrun file                |
| -------------------------------- | --------------------------- |
| `src/virtiofs/fuse.rs`           | `fs/fuse.rs`                |
| `src/virtiofs/filesystem.rs`     | `fs/filesystem.rs`          |
| `src/virtiofs/server.rs`         | `fs/server.rs`              |
| `src/virtiofs/multikey.rs`       | `fs/multikey.rs`            |
| `src/virtiofs/inode_alloc.rs`    | `fs/inode_alloc.rs`         |
| `src/virtiofs/passthrough.rs`    | `fs/macos/passthrough.rs`   |
| `src/virtiofs/fs_utils.rs`       | `fs/macos/fs_utils.rs`      |
| `src/virtiofs/queue.rs`          | `queue.rs`                  |
| `src/virtiofs/descriptor_utils.rs` | `descriptor_utils.rs`     |
| `src/virtiofs/file_traits.rs`    | `file_traits.rs`            |
| `src/virtiofs/linux_errno.rs`    | `linux_errno.rs`            |
| `src/virtiofs/bindings.rs`       | `bindings.rs`               |
| `src/virtiofs/worker_message.rs` | `utils/src/worker_message.rs` |

Changes made by vmmbox: module paths were rewritten to a flat module; the types
the code expected from the rest of libkrun's virtual machine monitor are
replaced by small definitions in `src/virtiofs/mod.rs`; the block-device
implementation was removed from `file_traits.rs`; the x86-only variants that
referred to KVM were removed from `worker_message.rs`. The rest is as it was.

libkrun is licensed under the Apache License, Version 2.0, the same license as
vmmbox (see `LICENSE`). Many of these files began life in Google's crosvm and
the Chromium OS project, and keep that copyright header; that code is under
the license below.

### Chromium OS / crosvm (BSD 3-Clause)

The source files carry "Copyright 2019 The Chromium OS Authors".

```text
Copyright 2017 The ChromiumOS Authors

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
   * Neither the name of Google Inc. nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
