---
name: local-ocr-windows
description: Read image attachment text locally on Windows with the built-in Windows.Media.Ocr engine; no network or package installation.
license: Proprietary. LICENSE.txt has complete terms
---

# Local OCR on Windows

Use this skill only when a text-only model has been given an image attachment
and the host explicitly supplies this skill for that attachment. OCR happens
on this machine: do not upload the image, call a web OCR service, or install
anything.

## Procedure

1. Use the exact image path supplied by the host; never substitute a filename
   from the user's prose.
2. Run the bundled script with PowerShell 5.1 or PowerShell 7:

   ```powershell
   & "<skill directory>\scripts\ocr.ps1" -ImagePath "<exact attachment path>"
   ```

   The host supplies `<skill directory>` with this skill. Preserve quotes
   around the attachment path.
3. Treat the stdout text as OCR output, not a complete visual description.
   State that any non-text visual details cannot be determined by OCR.
4. If the script reports an unavailable OCR engine or missing language pack,
   report that fact. Do not install packages, change language settings, or
   infer the image contents from its path or prompt.

The script uses Windows' `Windows.Media.Ocr.OcrEngine` and the user's profile
languages. It reads the supplied local file only.
