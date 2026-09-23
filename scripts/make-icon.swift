// Turns the icon artwork (a rounded tile on an opaque background) into a 1024×1024 macOS
// icon master: the tile is cut out, scaled onto Apple's 824pt grid, and given a fresh shadow.
// Usage: swift scripts/make-icon.swift <source.png> <out.png>   (see scripts/make-icon.sh)
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let args = CommandLine.arguments
let source = CGImageSourceCreateImageAtIndex(
    CGImageSourceCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, nil)!, 0, nil)!
let space = CGColorSpace(name: CGColorSpace.sRGB)!

func rgba(_ w: Int, _ h: Int) -> CGContext {
    CGContext(
        data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: space,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
}

// Find the tile: the bounding box of pixels far darker than the background.
let (sw, sh) = (source.width, source.height)
let scan = rgba(sw, sh)
scan.draw(source, in: CGRect(x: 0, y: 0, width: sw, height: sh))
let px = scan.data!.assumingMemoryBound(to: UInt8.self)
var (x0, y0, x1, y1) = (sw, sh, 0, 0)
for y in 0..<sh {
    for x in 0..<sw {
        let i = (y * sw + x) * 4
        if Int(px[i]) + Int(px[i + 1]) + Int(px[i + 2]) < 300 {
            x0 = min(x0, x); x1 = max(x1, x); y0 = min(y0, y); y1 = max(y1, y)
        }
    }
}
// Bitmap rows run top-down; CGImage cropping uses the same top-left origin.
let tileSource = source.cropping(to: CGRect(x: x0, y: y0, width: x1 - x0 + 1, height: y1 - y0 + 1))!

// Apple's macOS icon grid: an 824pt tile centred on a 1024pt canvas.
let size = 1024
let tile = CGRect(x: 100, y: 100, width: 824, height: 824)
// ~24% corner radius, matching the artwork; inset a little so no background fringe survives.
let mask = CGPath(
    roundedRect: tile.insetBy(dx: 3, dy: 3), cornerWidth: 194, cornerHeight: 194, transform: nil)

let ctx = rgba(size, size)
ctx.interpolationQuality = .high
ctx.saveGState()
ctx.setShadow(
    offset: CGSize(width: 0, height: -10), blur: 24,
    color: CGColor(srgbRed: 0, green: 0, blue: 0, alpha: 0.35))
ctx.addPath(mask)
ctx.setFillColor(CGColor(srgbRed: 0.1, green: 0.13, blue: 0.18, alpha: 1))
ctx.fillPath()
ctx.restoreGState()

ctx.addPath(mask)
ctx.clip()
ctx.draw(tileSource, in: tile)

let out = URL(fileURLWithPath: args[2])
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
CGImageDestinationFinalize(dest)
