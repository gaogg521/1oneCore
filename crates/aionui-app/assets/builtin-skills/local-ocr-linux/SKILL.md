---
name: local-ocr-linux
description: Read image attachment text locally on Linux using an already-installed Tesseract executable; no network or package installation.
license: Proprietary. LICENSE.txt has complete terms
---

# Local OCR on Linux

Use this skill only for an image attachment explicitly delegated to this
skill. The image must stay local: do not upload it, call an online OCR
service, or install a package.

## Procedure

1. Use the exact attachment path supplied by the host.
2. Run the bundled wrapper:

   ```bash
   "<skill directory>/scripts/ocr.sh" "<exact attachment path>"
   ```

3. The wrapper requires an already-installed `tesseract` and picks supported
   English/Chinese language data when present. Treat stdout as OCR text only.
4. If Tesseract or an appropriate language data file is missing, report that
   limitation and stop. Never run a package manager or infer image contents.
