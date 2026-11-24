# WinSizeP

中文 | [English](README.en.md)

Windows 窗口管理工具，通过 Alt 快捷键和鼠标操作快速移动、调整和排列窗口，适合多屏工作、录屏和演示。

## 功能

- 移动、缩放、居中和最大化窗口，支持多显示器。
- 用屏幕分区和右键九宫格快速排列窗口，可自定义分区与快捷键。
- 窗口置顶、十字线和待操作窗口标示。
- 鼠标与键盘操作标注，方便录屏和演示。
- 托盘暂停 / 启用捕获；设置自动保存，界面跟随 Windows 亮暗主题。
- 支持命令行操作、自动启动和管理员模式。

## 使用

运行 `winsizep.exe`，从托盘菜单打开设置。默认使用左 Alt：长按或双击后配合按键；鼠标操作针对按下 Alt 时的前台窗口。

| 常用操作 | 作用 |
| --- | --- |
| Alt + 左键拖动 | 移动窗口 |
| Alt + 右键拖动 | 调整窗口大小 |
| Alt + 右键点击 | 按九宫格排列窗口 |
| Alt + 中键点击 | 在鼠标所在屏幕居中 |
| Alt + F | 最大化；无法最大化时填满工作区 |
| Alt + Q/W/E、A/S/D、Z/X/C | 使用九个分区预设 |
| Alt + P | 置顶 / 取消置顶 |
| Alt + L | 显示 / 隐藏十字线 |
| Alt + M / Alt + K | 切换鼠标 / 键盘标注 |
| Alt + H | 查看当前快捷键帮助 |
| Esc | 关闭提示或取消操作 |

可在设置中调整快捷键、鼠标操作、分区和标注。更多操作见托盘“帮助”。控制管理员窗口时，需要以管理员身份运行 WinSizeP。

## 命令行

```powershell
.\winsizep.exe --name notepad.exe --center
.\winsizep.exe --pid 123 --location 100,200 --size 900,600
.\winsizep.exe --help
```

## 构建

需要 Windows x64、Rust 1.88+ MSVC 工具链和 Windows SDK。支持 Windows 10 1703 及更新版本。

```powershell
cargo build --release
cargo test
```

生成程序：`target/release/winsizep.exe`。
