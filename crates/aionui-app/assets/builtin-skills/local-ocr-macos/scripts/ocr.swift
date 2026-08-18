import AppKit
import Foundation
import Vision

guard CommandLine.arguments.count == 2 else {
    fputs("usage: ocr.swift <image-path>\n", stderr)
    exit(64)
}

let imageURL = URL(fileURLWithPath: CommandLine.arguments[1])
guard let image = NSImage(contentsOf: imageURL), let cgImage = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
    fputs("cannot decode local image: \(imageURL.path)\n", stderr)
    exit(65)
}

let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
request.usesLanguageCorrection = false

do {
    try VNImageRequestHandler(cgImage: cgImage, options: [:]).perform([request])
    let text = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }.joined(separator: "\n")
    print(text, terminator: "")
} catch {
    fputs("local Vision OCR failed: \(error)\n", stderr)
    exit(70)
}
