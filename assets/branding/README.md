# 品牌资源

- `logo.svg`：默认透明背景矢量 Logo（V 为深石板蓝 `#1E293B`，`viewBox="17 17 94 94"` 使主体接近画布边缘），Web 登录、顶栏与 favicon 共用。
- `icon.ico`：透明 SVG 派生的 Windows 16/24/32/48/64/128/256 图标，应用、窗口、托盘与 Explorer 同步根共用。
- `logo-dark.svg`、`icon-dark.ico`：S223 以前的原黑底版本，逐字节保留，供以后选用。

在仓库根目录、devShell 中使用宿主 ImageMagick 重新生成透明 ICO：

```bash
magick -background none -density 192 assets/branding/logo.svg -define icon:auto-resize=256,128,64,48,32,24,16 assets/branding/icon.ico
```

只修改默认源图时重生 `icon.ico`；备用黑底资源不随透明版重新生成。运行中的旧客户端需使用新构建；Explorer 由新 agent 正常恢复绑定时刷新内容寻址的持久图标，不依赖便携程序位置。
