# data-binary-sevenz

7z container facts, header indexing, and entry decoding.

The crate reads the 7z signature header, the (possibly encoded) main header, the
folder/coder graph, and the file table. It decodes entries through the internalized
pure-Rust codecs in `data-compression`: `Copy`, `LZMA`, `LZMA2`, and the `BCJ x86`
branch filter. Coders outside that set fail with an explicit unsupported-coder error;
no external process is ever spawned.

- `index_7z` / `index_7z_slice` build a `SevenZArchiveIndex` (version, coder methods,
  folders, entries, total unpack size) without materializing payload bytes.
- `decode_entry` materializes one entry under a `CodecLimits` output budget.
