from PIL import Image, ImageDraw, ImageFilter
import math

SIZE = 1024
img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# 圆角方形背景（深蓝紫渐变模拟：两色叠加）
R = 220
d.rounded_rectangle([64, 64, SIZE-64, SIZE-64], radius=R, fill=(30, 41, 59, 255))  # slate-900
# 上部高光
hl = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
dh = ImageDraw.Draw(hl)
dh.rounded_rectangle([64, 64, SIZE-64, SIZE-64], radius=R, fill=(99, 102, 241, 60))  # indigo tint
hl = hl.filter(ImageFilter.GaussianBlur(120))
img = Image.alpha_composite(img, hl)

d = ImageDraw.Draw(img)
# 白色托盘图形：三条横线（菜单列表）+ 底部圆角托盘
c = (255, 255, 255, 255)
bar_w = 560
bar_h = 56
x0 = (SIZE - bar_w) // 2
ys = [330, 484, 638]
for y in ys:
    d.rounded_rectangle([x0, y, x0 + bar_w, y + bar_h], radius=28, fill=c)
# 底部托盘底座
d.rounded_rectangle([x0 + 60, 792, x0 + bar_w - 60, 852], radius=30, fill=c)
import pathlib
out = pathlib.Path(__file__).resolve().parent.parent / "src-tauri" / "icons" / "app-icon.png"
img.save(out)
print("saved")
