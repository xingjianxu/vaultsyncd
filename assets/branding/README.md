# 品牌资源

- `logo.svg`：用户提供的横向画布矢量 Logo（1260×900，`viewBox="-25 159 1260 900"`，四周仅约2%留白）。鲜亮蓝色渐变云朵（`#0C60F1` → `#004DE5` → `#0039CA`）、向左上错位的浅蓝叠层（`#5794FA` → `#2E60D8`），以及珍珠白/浅柠檬渐变双箭头（`#FFFDF6`→`#E1EBF6`、`#FBFFE2`→`#DCF0A5`）。透明背景，形状均为纯矢量路径。Web 登录、顶栏与 favicon 共用；登录页高48px、顶栏高36px，按 `1260 / 900` 等比显示，不放进方形槽位拉伸。
- `icon.ico`：默认 SVG 派生的 Windows 16/24/32/48/64/128/256 图标，应用、窗口、托盘与 Explorer 同步根共用。源图为横向，生成时先居中补成透明正方形再缩放，故各尺寸保持云朵原始比例（宽占满、上下留透明带），不做非等比拉伸。
- `logo-dark.svg`、`icon-dark.ico`：S223 以前的原黑底版本，逐字节保留，供以后选用。

在仓库根目录、devShell 中使用宿主 ImageMagick 重新生成 ICO（先等比补成正方形，背景由 SVG 决定）：

```bash
magick -background none -density 192 assets/branding/logo.svg -resize 1024x1024 \
  -gravity center -background none -extent 1024x1024 PNG32:/tmp/vaultsyncd-icon-master.png
magick /tmp/vaultsyncd-icon-master.png \
  -define icon:auto-resize=256,128,64,48,32,24,16 assets/branding/icon.ico
```

直接用 `icon:auto-resize` 读横向 SVG 会把画面拉成正方形，勿省掉补齐步骤。只修改默认源图时重生 `icon.ico`；备用黑底资源不随默认版重新生成。运行中的旧客户端需使用新构建；Explorer 由新 agent 正常恢复绑定时刷新内容寻址的持久图标，不依赖便携程序位置。
