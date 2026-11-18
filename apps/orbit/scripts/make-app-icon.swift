// 生成 hiq 的应用图标。
//
// 原图标是黑底 + 白色「OP」（Orbit Pi 缩写）。这里沿用同样的视觉语言：
// macOS 风格圆角方形底 + 居中白色字标，只是文字换成 hiq。
//
// 用法：swift make-app-icon.swift <输出.png> [尺寸]
import AppKit
import CoreGraphics
import Foundation

let args = CommandLine.arguments
let outPath = args.count > 1 ? args[1] : "icon.png"
let size = args.count > 2 ? (Int(args[2]) ?? 1024) : 1024

let s = CGFloat(size)
let space = CGColorSpaceCreateDeviceRGB()
guard let ctx = CGContext(
    data: nil,
    width: size,
    height: size,
    bitsPerComponent: 8,
    bytesPerRow: 0,
    space: space,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else {
    FileHandle.standardError.write("建 CGContext 失败\n".data(using: .utf8)!)
    exit(1)
}

ctx.clear(CGRect(x: 0, y: 0, width: s, height: s))

// macOS 图标规范：圆角矩形约占画布 80%，四周留白（squircle 近似）
let inset = s * 0.10
let rect = CGRect(x: inset, y: inset, width: s - inset * 2, height: s - inset * 2)
let radius = rect.width * 0.2237 // 接近 macOS squircle 的圆角比例

let path = CGPath(
    roundedRect: rect,
    cornerWidth: radius,
    cornerHeight: radius,
    transform: nil
)
ctx.addPath(path)
ctx.setFillColor(CGColor(srgbRed: 0.04, green: 0.04, blue: 0.05, alpha: 1.0))
ctx.fillPath()

// 字标
let ns = NSGraphicsContext(cgContext: ctx, flipped: false)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = ns

let fontSize = rect.width * 0.46
let font = NSFont.systemFont(ofSize: fontSize, weight: .bold)
let attrs: [NSAttributedString.Key: Any] = [
    .font: font,
    .foregroundColor: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
    .kern: fontSize * 0.01,
]
let text = "hiq" as NSString
let ts = text.size(withAttributes: attrs)
text.draw(
    at: CGPoint(
        x: rect.midX - ts.width / 2,
        y: rect.midY - ts.height / 2
    ),
    withAttributes: attrs
)

NSGraphicsContext.restoreGraphicsState()

guard let image = ctx.makeImage() else { exit(1) }
let rep = NSBitmapImageRep(cgImage: image)
let data = rep.representation(using: .png, properties: [:])!
try data.write(to: URL(fileURLWithPath: outPath))
print("已写入 \(outPath)  \(size)x\(size)")
