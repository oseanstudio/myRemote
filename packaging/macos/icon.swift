import CoreFoundation
import CoreGraphics
import Foundation
import ImageIO

let size = 1024
let space = CGColorSpace(name: CGColorSpace.sRGB)!
let image = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                      bytesPerRow: size * 4, space: space,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

func color(_ rgb: UInt32, opacity: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat((rgb >> 16) & 255) / 255,
            green: CGFloat((rgb >> 8) & 255) / 255,
            blue: CGFloat(rgb & 255) / 255, alpha: opacity)
}

func rounded(_ rect: CGRect, _ radius: CGFloat) -> CGPath {
    CGPath(roundedRect: rect, cornerWidth: radius, cornerHeight: radius, transform: nil)
}

func fill(_ path: CGPath, _ rgb: UInt32, opacity: CGFloat = 1) {
    image.addPath(path)
    image.setFillColor(color(rgb, opacity: opacity))
    image.fillPath()
}

func gradient(_ path: CGPath, _ top: UInt32, _ bottom: UInt32, _ bounds: CGRect) {
    image.saveGState()
    image.addPath(path)
    image.clip()
    let colors = [color(top), color(bottom)] as CFArray
    image.drawLinearGradient(CGGradient(colorsSpace: space, colors: colors, locations: [0, 1])!,
                             start: CGPoint(x: bounds.minX, y: bounds.maxY),
                             end: CGPoint(x: bounds.maxX, y: bounds.minY), options: [])
    image.restoreGState()
}

let tile = CGRect(x: 68, y: 68, width: 888, height: 888)
gradient(rounded(tile, 208), 0x478BFA, 0x123999, tile)

let body = CGRect(x: 344, y: 180, width: 336, height: 665)
let remote = rounded(body, 105)
image.saveGState()
image.setShadow(offset: CGSize(width: 4, height: -18), blur: 38, color: color(0x092050, opacity: 0.58))
fill(remote, 0xD4E4FF)
image.restoreGState()
gradient(remote, 0xFFFFFF, 0xD0E4FF, body)

let pad = CGRect(x: 389, y: 546, width: 246, height: 246)
gradient(CGPath(ellipseIn: pad, transform: nil), 0x397CED, 0x143EAB, pad)
fill(CGPath(ellipseIn: CGRect(x: 465, y: 622, width: 94, height: 94), transform: nil), 0xEBF4FF)
for x in [420, 544] {
    fill(rounded(CGRect(x: CGFloat(x), y: 399, width: 60, height: 60), 24), 0x2154B7)
}
fill(rounded(CGRect(x: 426, y: 285, width: 172, height: 54), 25), 0x2154B7)

let destination = CGImageDestinationCreateWithURL(URL(fileURLWithPath: CommandLine.arguments[1]) as CFURL,
                                                  "public.png" as CFString, 1, nil)!
CGImageDestinationAddImage(destination, image.makeImage()!, nil)
precondition(CGImageDestinationFinalize(destination))
