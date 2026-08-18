[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ImagePath
)

$ErrorActionPreference = 'Stop'

# Windows PowerShell does not necessarily load this projection until it is
# requested. It is part of Windows/.NET, not a downloaded OCR dependency.
Add-Type -AssemblyName System.Runtime.WindowsRuntime

function Await-WinRtOperation {
    param(
        [Parameter(Mandatory = $true)]$Operation,
        [Parameter(Mandatory = $true)][Type]$ResultType
    )

    $method = [System.WindowsRuntimeSystemExtensions].GetMethods() |
        Where-Object {
            $_.Name -eq 'GetAwaiter' -and
            $_.IsGenericMethodDefinition -and
            $_.GetParameters().Count -eq 1 -and
            $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
        } |
        Select-Object -First 1

    if ($null -eq $method) {
        throw 'Windows Runtime async support is unavailable in this PowerShell host.'
    }

    return $method.MakeGenericMethod($ResultType).Invoke($null, @($Operation)).GetResult()
}

$normalizedImagePath = $ImagePath
if ($normalizedImagePath -match '^[\\]+\?[\\]+UNC[\\]+') {
    # Bash/ACP-to-PowerShell quoting can collapse one leading slash. Accept
    # both `\\?\UNC\server\share` and `\?\UNC\server\share`.
    $normalizedImagePath = '\\' + [regex]::Replace(
        $normalizedImagePath,
        '^[\\]+\?[\\]+UNC[\\]+',
        '',
        [System.Text.RegularExpressions.RegexOptions]::IgnoreCase
    )
}
elseif ($normalizedImagePath -match '^[\\]+\?[\\]+') {
    # The chat attachment pipeline preserves Win32 extended-length paths. The
    # WinRT file API accepts a normal absolute path, while Resolve-Path does
    # not recognize `\\?\C:` as a PowerShell drive.
    $normalizedImagePath = [regex]::Replace($normalizedImagePath, '^[\\]+\?[\\]+', '')
}
$path = (Resolve-Path -LiteralPath $normalizedImagePath).Path

# Load the exact WinRT projections used below. This does not download or
# install anything; Windows supplies the OCR engine and language packs.
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Storage.FileAccessMode, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.IRandomAccessStream, Windows.Storage.Streams, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.SoftwareBitmap, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
$null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
$null = [Windows.Media.Ocr.OcrResult, Windows.Foundation, ContentType = WindowsRuntime]

$engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
if ($null -eq $engine) {
    throw 'Windows.Media.Ocr has no recognizer for the current user profile languages.'
}

$file = Await-WinRtOperation -Operation ([Windows.Storage.StorageFile]::GetFileFromPathAsync($path)) -ResultType ([Windows.Storage.StorageFile])
$stream = Await-WinRtOperation -Operation ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) -ResultType ([Windows.Storage.Streams.IRandomAccessStream])
try {
    $decoder = Await-WinRtOperation -Operation ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) -ResultType ([Windows.Graphics.Imaging.BitmapDecoder])
    $bitmap = Await-WinRtOperation -Operation ($decoder.GetSoftwareBitmapAsync()) -ResultType ([Windows.Graphics.Imaging.SoftwareBitmap])
    try {
        $result = Await-WinRtOperation -Operation ($engine.RecognizeAsync($bitmap)) -ResultType ([Windows.Media.Ocr.OcrResult])
        [Console]::Out.Write($result.Text)
    }
    finally {
        $bitmap.Dispose()
    }
}
finally {
    $stream.Dispose()
}
