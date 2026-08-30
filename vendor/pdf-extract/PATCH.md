# learnive's patched `pdf-extract`

Vendored copy of `pdf-extract` 0.12.0 (upstream:
https://github.com/jrmuizel/pdf-extract, MIT), pinned via
`[patch.crates-io]` in the workspace root `Cargo.toml`. Source is otherwise
untouched; the only change is in `src/lib.rs`, `Processor::process_stream`.

## The bug

`process_stream`'s content-stream operator dispatch indexes
`operation.operands[N]` unconditionally (or `assert!`s the exact operand
count) for operators like `w`, `m`, `l`, `c`, `v`, `y`, `re`, `cm`, `Tm`,
`Td`, `TD`, `Tf`, `gs`, `Do`, etc. A malformed/truncated operator with fewer
operands than expected — which real-world PDFs do contain, confirmed live
against a 1,308-page textbook where 12 pages hit this — panics instead of
returning an `Err`.

Worse: because a page's operations are processed sequentially in one pass,
the panic on operation N discards the text already extracted from
operations 1..N-1 on that same page, and (if not caught page-by-page by the
caller) can discard every other page's text too if the panic happens inside
a whole-document call.

## The fix

A single guard (`min_operands`, added just before `impl<'a> Processor<'a>`,
and the length check at the top of the `for operation in &content.operations`
loop in `process_stream`) skips a malformed operation — with a `warn!` log —
instead of indexing into or asserting on operands that aren't there. All of
the operators this protects are graphics-state/path-construction operators
that pdf-extract doesn't use for rendering (it's a text extractor, not a
renderer), so skipping a malformed one has no effect on extracted text
correctness; it only prevents the panic.

Net effect: a malformed operator anywhere in a content stream no longer
loses any other text on the page (or document) around it. Verified against
the real 1,308-page book: all 1,308 pages' text is now recovered, including
the 12 pages that previously panicked and lost everything downstream of
them.

learnive's own `source::pdf::read_pdf`/`source::extract_pdf_text` still wrap
per-page extraction in `catch_unwind` as defense-in-depth (this patch fixes
every operator class observed so far, not a formal proof there is no other
panic path in the crate) — see PLAN.md's S27 pdf-extract entries.

---

# Patch 2 — PDF comments in content streams (2026-08-30)

## The bug

`Processor::process_stream` hands the raw page content stream to
`lopdf::content::Content::decode`. `lopdf`'s decoder does not handle the
PDF comment token (`%` to end of line) and **abandons the entire stream**,
returning `Ok` with **zero operations**. It is a silent, total loss: no
error, no warning, just a page that extracts as the empty string.

Real scanners emit these comments. Every page of the 1978 K&R scan in the
test library begins with `% CANON_PFINF_TYPE2_TEXTON` — a Canon scanner
marker — and that single comment cost the whole book its text: **236 of
236 pages empty**, while poppler's `pdftotext` read the same file fine.
The file is not damaged and its OCR text layer is perfectly good
(`3 Tr` invisible text, non-embedded WinAnsi Helvetica, hex-encoded ASCII).

Measured on a representative page: `Content::decode` returns **0**
operations as-is and **505** with comments stripped.

This is a whole class of books, not one file — it hits any PDF whose
producer writes a comment into the content stream.

## Why it mattered more than a normal extraction bug

The acervo gate (SPEC §11.1) reads an empty extraction as "no text layer"
and tells the user to acquire a different copy. Under manual acquisition
the user pays for every download by hand, so a false rejection sends them
to re-download a book that was already correct.

## The fix

`strip_content_comments` (added just before `min_operands`), called at the
top of `process_stream`. It removes `%`-to-end-of-line comments while
tracking literal-string state, so a `%` inside `( … )` — real page text —
is preserved rather than truncating the rest of the stream. Parenthesis
nesting and backslash escapes are handled per the PDF spec.

Result on K&R: 0 chars → **430,525 chars across 236 pages**, 8 empty.

Covered by `source::toc_bench::kr_text_layer` (`#[ignore]`, needs the real
library file) and `source::pdf`'s own unit tests.
