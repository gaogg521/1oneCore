#!/usr/bin/env sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: ocr.sh <image-path>" >&2
  exit 64
fi

if ! command -v tesseract >/dev/null 2>&1; then
  echo "local OCR unavailable: tesseract is not installed" >&2
  exit 69
fi

languages="$(tesseract --list-langs 2>/dev/null || true)"
if printf '%s\n' "$languages" | grep -qx 'chi_sim' && printf '%s\n' "$languages" | grep -qx 'eng'; then
  lang='chi_sim+eng'
elif printf '%s\n' "$languages" | grep -qx 'eng'; then
  lang='eng'
else
  echo "local OCR unavailable: no supported English or Simplified Chinese Tesseract language data" >&2
  exit 69
fi

exec tesseract "$1" stdout -l "$lang"
