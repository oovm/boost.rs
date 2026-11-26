# `data-binary-container`

CPython bytecode, PyInstaller, NSIS, asar and Tauri container facts.

This crate owns the container parsers that OpenIDA and OpenCTF previously carried
twice, once per product:

- `pyc` / `pyc_opcodes`: the CPython `marshal` object graph, 3.11 through 3.13
  bytecode and the per-version opcode tables.
- `pyinstaller`: the `CArchive` cookie/TOC framing and the `PYZ` module archive.
- `nsis`: the first header, solid framing, the eight header blocks, the script
  command stream and the `File` extraction manifest.
- `asar`: the pickle header and the JSON file tree of Electron archives.
- `tauri`: the interleaved `(key, blob)` asset table of Tauri v2 executables.

Every entry point reads a caller-provided container image and reports offsets
relative to that image, so the same facts serve both products without a session
facade. Decompression stays delegated to `data-compression` (`Deflate`, `Zlib`,
`Lzma1`, `Brotli`); this crate only frames the streams and applies the byte
budgets.
