// 把截图里的文字读出来。界面探针靠它判断"到底渲染出了什么"，
// 而不是靠人眼——人眼验证过一次就过期了，OCR 每次都能重跑。
import Foundation
import Vision
import AppKit

guard CommandLine.arguments.count > 1,
      let img = NSImage(contentsOfFile: CommandLine.arguments[1]),
      let cg = img.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
    print("ERR: cannot load"); exit(1)
}
let req = VNRecognizeTextRequest()
req.recognitionLevel = .accurate
req.recognitionLanguages = ["zh-Hans", "en-US"]
try VNImageRequestHandler(cgImage: cg, options: [:]).perform([req])
let obs = (req.results ?? []).sorted { a, b in
    let ay = 1 - a.boundingBox.midY, by = 1 - b.boundingBox.midY
    return abs(ay - by) > 0.012 ? ay < by : a.boundingBox.minX < b.boundingBox.minX
}
for o in obs { if let c = o.topCandidates(1).first { print(c.string) } }
