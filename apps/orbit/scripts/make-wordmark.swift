// 生成 hiq 的 wordmark（侧栏顶部品牌图）。
//
// 关键：画布要**紧贴文字**。原先固定 1892×410 的画布，字只占中间一小块，
// 缩放到界面里 76px 宽时字就小得看不清了。这里先量文字实际尺寸，
// 再按它加一点内边距开画布，保证缩放后字撑满可用宽度。
//
// 用法：swift make-wordmark.swift <输出路径> <画布高> <颜色>
//   例如 swift make-wordmark.swift logo.png 410 "#FFFFFF"

import AppKit
import CoreGraphics
import Foundation

let args = CommandLine.arguments
guard args.count >= 4 else {
    FileHandle.standardError.write("用法: make-wordmark.swift <out> <height> <#RRGGBB>\n".data(using: .utf8)!)
    exit(2)
}

let outPath = args[1]
let canvasHeight = Int(args[2]) ?? 410
let hex = args[3]

var rgb: UInt64 = 0xFFFFFF
if hex.hasPrefix("#") {
    Scanner(string: String(hex.dropFirst())).scanHexInt64(&rgb)
}
let r = CGFloat((rgb >> 16) & 0xFF) / 255.0
let g = CGFloat((rgb >> 8) & 0xFF) / 255.0
let b = CGFloat(rgb & 0xFF) / 255.0

// 字号取画布高的 ~92%：字几乎占满高度，配合较宽的字距，
// 在 76px 宽的显示尺寸下仍清晰（这正是原 wordmark 1892×410 的比例策略）。
let fontSize = CGFloat(canvasHeight) * 0.52
let font = NSFont.systemFont(ofSize: fontSize, weight: .bold)
let attrs: [NSAttributedString.Key: Any] = [
    .font: font,
    .foregroundColor: NSColor(srgbRed: r, green: g, blue: b, alpha: 1.0),
    // 拉开字距让字形舒展，接近原图那种横向铺开的感觉
    .kern: fontSize * 0.06,
]
let text = "hiq" as NSString
let textSize = text.size(withAttributes: attrs)

// 画布尺寸决定它在界面里显示多大。
//
// 界面用 `embedded_image_w(..., px(76.))` 按**宽度**定尺寸并等比缩放，
// 所以画布的宽高比直接决定字的实际高度：
//   - 画布紧贴文字（382×282）→ 高宽比 0.74 → 76px 宽时字高 56px，太大太抢眼；
//   - 原 wordmark（1892×410）→ 高宽比 0.22 → 字高仅 16px，小到看不清。
// 取中间值 0.40：76px 宽时字高约 30px，既清晰又不喧宾夺主。
const H_OVER_W: CGFloat = 0.40;
let canvasWidth = Int((textSize.height / H_OVER_W).rounded(.up))
let padX = (CGFloat(canvasWidth) - textSize.width) / 2.0
let height = canvasHeight

let space = CGColorSpaceCreateDeviceRGB()
guard let ctx = CGContext(
    data: nil,
    width: canvasWidth,
    height: height,
    bitsPerComponent: 8,
    bytesPerRow: 0,
    space: space,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else {
    FileHandle.standardError.write("建 CGContext 失败\n".data(using: .utf8)!)
    exit(1)
}
ctx.clear(CGRect(x: 0, y: 0, width: canvasWidth, height: height))

let ns = NSGraphicsContext(cgContext: ctx, flipped: false)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = ns

let origin = CGPoint(
    x: (CGFloat(canvasWidth) - textSize.width) / 2.0,
    y: (CGFloat(height) - textSize.height) / 2.0
)
text.draw(at: origin, withAttributes: attrs)

NSGraphicsContext.restoreGraphicsState()

guard let image = ctx.makeImage() else { exit(1) }
let rep = NSBitmapImageRep(cgImage: image)
let data = rep.representation(using: .png, properties: [:])!
try data.write(to: URL(fileURLWithPath: outPath))
print("已写入 \(outPath)  \(canvasWidth)x\(height)  (文字 \(Int(textSize.width))x\(Int(textSize.height)))")
