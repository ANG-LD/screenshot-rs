#!/usr/bin/env python3
"""生成 screenshot-rs 应用图标 PNG 文件（纯 Python / 无外部依赖）"""
import struct, zlib, math, os

def png_chunk(chunk_type: bytes, data: bytes) -> bytes:
    c = chunk_type + data
    return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

def make_png(w: int, h: int, rgba: bytes) -> bytes:
    """从 RGBA 字节数组生成 PNG 文件"""
    assert len(rgba) == w * h * 4
    # 每一行前面加 filter byte 0 (None)
    raw = b""
    for row in range(h):
        raw += b"\x00" + rgba[row * w * 4 : (row + 1) * w * 4]
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + png_chunk(b"IDAT", zlib.compress(raw))
        + png_chunk(b"IEND", b"")
    )

def fill_rounded_rect(pixels: bytearray, stride: int, w: int, h: int,
                      x: int, y: int, rw: int, rh: int, radius: int,
                      r: int, g: int, b: int, a: int):
    """填充一个圆角矩形（抗锯齿边缘）"""
    # 使用距离场：每个像素到圆角矩形的最短距离
    for py in range(max(0, y - 1), min(h, y + rh + 1)):
        for px in range(max(0, x - 1), min(w, x + rw + 1)):
            # 计算到圆角矩形边界的距离
            left = x + radius
            right = x + rw - radius
            top = y + radius
            bottom = y + rh - radius
            cx = max(left, min(px, right))
            cy = max(top, min(py, bottom))
            d = math.sqrt((px - cx) ** 2 + (py - cy) ** 2)
            # 如果像素在矩形内部
            if left <= px <= right or top <= py <= bottom:
                dist = d - radius
            elif (px < left and py < top):          # 左上角
                dist = math.sqrt((px - left) ** 2 + (py - top) ** 2) - radius
            elif (px > right and py < top):          # 右上角
                dist = math.sqrt((px - right) ** 2 + (py - top) ** 2) - radius
            elif (px < left and py > bottom):        # 左下角
                dist = math.sqrt((px - left) ** 2 + (py - bottom) ** 2) - radius
            elif (px > right and py > bottom):       # 右下角
                dist = math.sqrt((px - right) ** 2 + (py - bottom) ** 2) - radius
            else:
                continue
            if dist < 1.0:
                # 抗锯齿混合
                alpha = min(1.0, max(0.0, 1.0 - dist)) * a
                idx = (py * stride + px) * 4
                old_a = pixels[idx + 3]
                if old_a == 0:
                    pixels[idx + 0] = r
                    pixels[idx + 1] = g
                    pixels[idx + 2] = b
                    pixels[idx + 3] = int(alpha)
                else:
                    na = alpha + old_a * (1 - alpha / 255.0)
                    pixels[idx + 0] = int((r * alpha + pixels[idx + 0] * old_a * (1 - alpha / 255.0)) / na) if na > 0 else 0
                    pixels[idx + 1] = int((g * alpha + pixels[idx + 1] * old_a * (1 - alpha / 255.0)) / na) if na > 0 else 0
                    pixels[idx + 2] = int((b * alpha + pixels[idx + 2] * old_a * (1 - alpha / 255.0)) / na) if na > 0 else 0
                    pixels[idx + 3] = int(min(255, na))

def draw_line(pixels: bytearray, stride: int, x1: float, y1: float, x2: float, y2: float,
              w: int, h: int, r: int, g: int, b: int, a: int, width: float):
    """画一条抗锯齿线段"""
    dx = x2 - x1
    dy = y2 - y1
    length = math.sqrt(dx * dx + dy * dy)
    if length < 0.5:
        return
    steps = int(length * 2)
    for i in range(steps + 1):
        t = i / steps
        cx = x1 + dx * t
        cy = y1 + dy * t
        for py in range(max(0, int(cy - width)), min(h, int(cy + width + 1))):
            for px in range(max(0, int(cx - width)), min(w, int(cx + width + 1))):
                dist = math.sqrt((px - cx) ** 2 + (py - cy) ** 2)
                if dist < width:
                    alpha = max(0.0, min(1.0, width - dist)) * a / width
                    idx = (py * stride + px) * 4
                    old_a = pixels[idx + 3]
                    if old_a == 0:
                        pixels[idx + 0] = r
                        pixels[idx + 1] = g
                        pixels[idx + 2] = b
                        pixels[idx + 3] = int(alpha)
                    else:
                        blend = alpha / 255.0
                        pixels[idx + 0] = int(pixels[idx + 0] * (1 - blend) + r * blend)
                        pixels[idx + 1] = int(pixels[idx + 1] * (1 - blend) + g * blend)
                        pixels[idx + 2] = int(pixels[idx + 2] * (1 - blend) + b * blend)
                        pixels[idx + 3] = min(255, old_a + int(alpha))

def draw_circle(pixels: bytearray, stride: int, w: int, h: int,
                cx: float, cy: float, radius: float, stroke_width: float,
                r: int, g: int, b: int, a: int):
    """画一个抗锯齿空心圆"""
    for py in range(max(0, int(cy - radius - stroke_width)), min(h, int(cy + radius + stroke_width + 1))):
        for px in range(max(0, int(cx - radius - stroke_width)), min(w, int(cx + radius + stroke_width + 1))):
            dist = abs(math.sqrt((px - cx) ** 2 + (py - cy) ** 2) - radius)
            if dist < stroke_width:
                alpha = max(0.0, min(1.0, stroke_width - dist)) * a / stroke_width
                idx = (py * stride + px) * 4
                old_a = pixels[idx + 3]
                if old_a == 0:
                    pixels[idx + 0] = r
                    pixels[idx + 1] = g
                    pixels[idx + 2] = b
                    pixels[idx + 3] = int(alpha)
                else:
                    blend = alpha / 255.0
                    pixels[idx + 0] = int(pixels[idx + 0] * (1 - blend) + r * blend)
                    pixels[idx + 1] = int(pixels[idx + 1] * (1 - blend) + g * blend)
                    pixels[idx + 2] = int(pixels[idx + 2] * (1 - blend) + b * blend)
                    pixels[idx + 3] = min(255, old_a + int(alpha))

def lerp_color(t: float) -> tuple:
    """渐变：深蓝 (#4361EE) -> 紫 (#5E5CE6) -> 紫罗兰 (#7B2FF7)"""
    if t < 0.5:
        s = t / 0.5
        return (
            int(0x43 + (0x5E - 0x43) * s),
            int(0x61 + (0x5C - 0x61) * s),
            int(0xEE + (0xE6 - 0xEE) * s),
        )
    else:
        s = (t - 0.5) / 0.5
        return (
            int(0x5E + (0x7B - 0x5E) * s),
            int(0x5C + (0x2F - 0x5C) * s),
            int(0xE6 + (0xF7 - 0xE6) * s),
        )

def fill_gradient_rect(pixels: bytearray, stride: int, w: int, h: int,
                       x: int, y: int, rw: int, rh: int, radius: int):
    """填充渐变圆角矩形（先画渐变底，再做圆角裁剪）"""
    for py in range(y, y + rh):
        for px in range(x, x + rw):
            t = ((px - x) / rw + (py - y) / rh) / 2.0  # 对角线渐变
            r, g, b = lerp_color(t)
            fill_rounded_rect(pixels, stride, w, h, px, py, 1, 1, 0, r, g, b, 255)

def make_icon(size: int) -> bytes:
    """生成给定尺寸的图标 PNG"""
    w = h = size
    stride = w
    pixels = bytearray(w * h * 4)

    # 缩放因子（以 128px 为基准）
    s = size / 128.0

    # 投影
    fill_rounded_rect(pixels, stride, w, h,
        int(14 * s), int(16 * s), int(104 * s), int(104 * s), int(26 * s),
        0, 0, 0, 38)

    # 主体背景渐变 (逐行近似)
    for py in range(int(12 * s), int(116 * s)):
        for px in range(int(12 * s), int(116 * s)):
            t = ((px - 12 * s) / (104 * s) + (py - 12 * s) / (104 * s)) / 2.0
            r, g, b = lerp_color(t)
            fill_rounded_rect(pixels, stride, w, h, px, py, 1, 1, 0, r, g, b, 255)

    # 圆角裁剪背景
    # 用 mask 方式：重新绘制背景为精确圆角矩形
    # 简化：画圆角矩形覆盖面
    pixels_bg = bytearray(w * h * 4)
    fill_rounded_rect(pixels_bg, stride, w, h,
        int(12 * s), int(12 * s), int(104 * s), int(104 * s), int(26 * s),
        0, 0, 0, 255)
    # 用 mask 裁剪渐变底
    for i in range(0, len(pixels), 4):
        if pixels_bg[i + 3] > 0:
            pass  # 保留
        else:
            pixels[i + 3] = 0  # 透明化圆角外的区域

    # 取景框
    fill_rounded_rect(pixels, stride, w, h,
        int(26 * s), int(24 * s), int(76 * s), int(80 * s), int(10 * s),
        255, 255, 255, 30)  # 先填充半透明白底
    # 描边
    # 简化：画圆角矩形轮廓
    for px in range(int(26 * s), int(102 * s)):
        for py in (int(24 * s), int(104 * s)):
            fill_rounded_rect(pixels, stride, w, h, px, py, 2, 2, 0, 255, 255, 255, 200)
    for py in range(int(24 * s), int(104 * s)):
        for px in (int(26 * s), int(102 * s)):
            fill_rounded_rect(pixels, stride, w, h, px, py, 2, 2, 0, 255, 255, 255, 200)

    # 四角 L 形取景器
    L_LEN = int(16 * s)
    L_WIDTH = max(2, int(5 * s))
    L_OFFSET = int(2 * s)
    corners = [
        # 左上
        (26, 40, 26, 24+L_OFFSET), (26, 24+L_OFFSET, 42, 24+L_OFFSET),
        # 右上
        (86, 24+L_OFFSET, 102, 24+L_OFFSET), (102, 24+L_OFFSET, 102, 40),
        # 左下
        (26, 88, 26, 104-L_OFFSET), (26, 104-L_OFFSET, 42, 104-L_OFFSET),
        # 右下
        (86, 104-L_OFFSET, 102, 104-L_OFFSET), (102, 104-L_OFFSET, 102, 88),
    ]
    for x1, y1, x2, y2 in corners:
        draw_line(pixels, stride,
            x1 * s, y1 * s, x2 * s, y2 * s,
            w, h, 255, 255, 255, 240, L_WIDTH)

    # 中心十字准星圆
    cx, cy = int(64 * s), int(64 * s)
    r = int(8 * s)
    sw = max(1.5, 2.0 * s)
    draw_circle(pixels, stride, w, h, cx, cy, r, sw, 255, 255, 255, 230)
    # 十字线
    CROSS_LEN = int(8 * s)
    CROSS_GAP = int(3 * s)
    CROSS_W = max(1.5, 2.5 * s)
    lines = [
        (cx, cy - r - CROSS_GAP, cx, cy - r - CROSS_GAP - CROSS_LEN),  # 上
        (cx, cy + r + CROSS_GAP, cx, cy + r + CROSS_GAP + CROSS_LEN),  # 下
        (cx - r - CROSS_GAP, cy, cx - r - CROSS_GAP - CROSS_LEN, cy),  # 左
        (cx + r + CROSS_GAP, cy, cx + r + CROSS_GAP + CROSS_LEN, cy),  # 右
    ]
    for x1, y1, x2, y2 in lines:
        draw_line(pixels, stride, x1, y1, x2, y2, w, h, 255, 255, 255, 230, CROSS_W)

    return make_png(w, h, bytes(pixels))

if __name__ == "__main__":
    os.chdir(os.path.dirname(os.path.abspath(__file__)))
    for size in [24, 48, 128, 256]:
        name = f"tray-{size}.png" if size <= 48 else f"app-{size}.png"
        png = make_icon(size)
        with open(name, "wb") as f:
            f.write(png)
        print(f"  ✓ {name} ({len(png)} bytes)")
    print("Done.")
