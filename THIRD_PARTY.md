# Third-party components

subsyncer is licensed under MIT OR Apache-2.0. The `subsyncer` binary and
Docker image also contain the following third-party components.

## Silero VAD model (embedded)

`crates/cli/assets/silero_vad.onnx`, v6.2.3, from https://github.com/snakers4/silero-vad

```
MIT License

Copyright (c) 2020-present Silero Team

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.```

## ONNX Runtime

Version 1.31.0, prebuilt by the `ort` crate and statically linked into the
`subsyncer` binary (release binaries and Docker image). ONNX Runtime bundles
further third-party code; Microsoft's notices for it are in
[`licenses/onnxruntime-ThirdPartyNotices.txt`](licenses/onnxruntime-ThirdPartyNotices.txt).

https://github.com/microsoft/onnxruntime

```
MIT License

Copyright (c) Microsoft Corporation

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Rust crates

All Rust dependencies compiled into `subsyncer` are under permissive licences
(MIT, Apache-2.0, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, BSL-1.0);
none are copyleft. `r-efi` also offers LGPL-2.1 but is MIT-licensed as used
here, and `webpki-roots` (CDLA-Permissive-2.0) runs only in the build script
that downloads ONNX Runtime. List them with
`cargo metadata --format-version 1 | jq -r '.packages[] | "\(.name) \(.license)"'`.

## FFmpeg

FFmpeg is not linked; subsyncer runs `ffmpeg`/`ffprobe` as separate processes.
The Docker image ships a minimal static FFmpeg built from the official source
release (version and SHA-256 pinned in the `Dockerfile`) by
`docker/build-ffmpeg.sh`. That configuration enables no GPL or non-free
components, so those binaries are LGPL-2.1-or-later. Their source is the pinned
release tarball from https://ffmpeg.org/releases/ plus that build script.

## Where these notices ship

Release archives include this file and the `licenses/` directory. In the Docker
image they are in `/usr/share/doc/subsyncer/`, together with FFmpeg's
`COPYING.LGPLv2.1` and `LICENSE.md`.
