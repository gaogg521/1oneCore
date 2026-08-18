---
name: local-ocr-macos
description: Read image attachment text locally on macOS with the built-in Vision framework; no network or package installation.
license: Proprietary. LICENSE.txt has complete terms
---

# Local OCR on macOS

Use this skill only for an image attachment explicitly delegated to this
skill. Keep the file local: do not upload it, use an online OCR service, or
install dependencies.

## Procedure

1. Use the exact attachment path supplied by the host.
2. Verify the Command Line Tools are already available, then run:

   ```bash
   xcrun --find swift
   swift "<skill directory>/scripts/ocr.swift" "<exact attachment path>"
   ```

3. The script uses Apple's on-device Vision framework. Treat its stdout as
   extracted text only; do not claim it describes non-text image content.
4. If `swift`, Vision, or a language is unavailable, say so. Do not invoke an
   installer and do not infer the image content from contextual clues.
